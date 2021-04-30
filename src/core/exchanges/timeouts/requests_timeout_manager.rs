use std::collections::HashMap;

use anyhow::{anyhow, Result};
use chrono::Duration;
use log::{error, info};
use uuid::Uuid;

use crate::core::{
    exchanges::common::ExchangeAccountId, exchanges::general::request_type::RequestType, DateTime,
};

use super::{
    more_or_equals_available_requests_count_trigger_scheduler::MoreOrEquelsAvailableRequestsCountTriggerScheduler,
    pre_reserved_group::PreReservedGroup, request::Request,
    requests_counts_in_period_result::RequestsCountsInPeriodResult,
    triggers::handle_trigger_trait::TriggerHandler,
};

pub struct RequestsTimeoutManager {
    requests_per_period: usize,
    period_duration: Duration,
    exchange_account_id: ExchangeAccountId,
    pub requests: Vec<Request>,
    pre_reserved_groups: Vec<PreReservedGroup>,
    last_time: Option<DateTime>,

    pub group_was_reserved: Box<dyn FnMut(PreReservedGroup)>,
    pub group_was_removed: Box<dyn FnMut(PreReservedGroup)>,

    less_or_equals_requests_count_triggers: Vec<Box<dyn TriggerHandler>>,
    more_or_equals_available_requests_count_trigger_scheduler:
        MoreOrEquelsAvailableRequestsCountTriggerScheduler,
    // delay_to_next_time_period: Duration,
    // data_recorder
}

impl RequestsTimeoutManager {
    pub fn new(
        requests_per_period: usize,
        period_duration: Duration,
        exchange_account_id: ExchangeAccountId,
        more_or_equals_available_requests_count_trigger_scheduler: MoreOrEquelsAvailableRequestsCountTriggerScheduler,
    ) -> Self {
        Self {
            requests_per_period,
            period_duration,
            exchange_account_id,
            requests: Default::default(),
            pre_reserved_groups: Default::default(),
            last_time: None,
            group_was_reserved: Box::new(|_| {}),
            group_was_removed: Box::new(|_| {}),
            less_or_equals_requests_count_triggers: Default::default(),
            more_or_equals_available_requests_count_trigger_scheduler,
        }
    }

    pub fn try_reserve_group(
        &mut self,
        group_type: String,
        current_time: DateTime,
        requests_count: usize,
        // call_source: SourceInfo, // TODO not needed until DataRecorder is ready
    ) -> Result<Option<Uuid>> {
        // FIXME lock maybe

        let current_time = self.get_non_decreasing_time(current_time);
        self.remove_outdated_requests(current_time)?;

        let _all_available_requests_count = self.get_all_available_requests_count();
        let available_requests_count = self.get_available_requests_count_at_persent(current_time);

        if available_requests_count < requests_count {
            // TODO save to DataRecorder
            return Ok(None);
        }

        let group_id = Uuid::new_v4();
        let group = PreReservedGroup::new(group_id, group_type, requests_count);
        self.pre_reserved_groups.push(group.clone());

        // TODO Why do we need some specific logger?
        info!("PreReserved grop {} {} was added", group_id, requests_count);

        // TODO save to DataRecorder

        self.last_time = Some(current_time);

        (self.group_was_reserved)(group);

        Ok(Some(group_id))
    }

    pub fn remove_group(&mut self, group_id: Uuid, _current_time: DateTime) -> bool {
        // FIXME outer lock

        let _all_available_requests_count = self.get_all_available_requests_count();
        let stored_group = self
            .pre_reserved_groups
            .iter()
            .position(|group| group.id == group_id);

        match stored_group {
            None => {
                // FIXME Why some special logger?
                error!("Cannot find PreReservedGroup {} for removing", { group_id });
                // TODO save to DataRecorder

                false
            }
            Some(group_index) => {
                let group = self.pre_reserved_groups[group_index].clone();
                let pre_reserved_requests_count = group.pre_reserved_requests_count;
                self.pre_reserved_groups.remove(group_index);

                info!(
                    "PreReservedGroup {} {} was removed",
                    group_id, pre_reserved_requests_count
                );

                // TODO save to DataRecorder

                (self.group_was_removed)(group);

                true
            }
        }
    }

    fn try_reserve_instant(
        &mut self,
        request_type: RequestType,
        current_time: DateTime,
        pre_reserved_group_id: Option<Uuid>,
        // FIXME Some logger context?
    ) -> Result<bool> {
        match pre_reserved_group_id {
            Some(pre_reserved_group_id) => {
                self.try_reserve_group_instant(request_type, current_time)
            }
            None => {
                self.try_reserve_request_instant(request_type, current_time, pre_reserved_group_id)
            }
        }
    }

    pub fn try_reserve_group_instant(
        &mut self,
        request_type: RequestType,
        current_time: DateTime,
    ) -> Result<bool> {
        // FIXME delete it
        Ok(false)
    }

    pub fn try_reserve_request_instant(
        &mut self,
        request_type: RequestType,
        current_time: DateTime,
        pre_reserved_group_id: Option<Uuid>,
    ) -> Result<bool> {
        // FIXME outer lock probably
        let current_time = self.get_non_decreasing_time(current_time);
        self.remove_outdated_requests(current_time)?;

        let all_available_requests_count = self.get_all_available_requests_count();
        let available_requests_count = self.get_available_requests_count_at_persent(current_time);

        if available_requests_count <= 0 {
            // TODO save to DataRecorder

            return Ok(false);
        }

        let request = self.add_request(request_type, current_time, None);

        // FIXME delete it
        Ok(false)
    }

    fn add_request(
        &mut self,
        request_type: RequestType,
        current_time: DateTime,
        group_id: Option<Uuid>,
    ) -> Result<Request> {
        let request = Request::new(request_type, current_time, group_id);

        let request_index = self.requests.binary_search_by(|stored_request| {
            stored_request
                .allowed_start_time
                .cmp(&request.allowed_start_time)
        });

        let request_index =
            request_index.map_or_else(|error_index| error_index, |ok_index| ok_index);

        self.requests.insert(request_index, request.clone());

        self.handle_all_decreasing_triggers();
        self.handle_all_increasing_triggers()?;

        Ok(request)
    }

    fn handle_all_decreasing_triggers(&mut self) {
        let available_requests_count = self.get_all_available_requests_count();
        self.less_or_equals_requests_count_triggers
            .iter_mut()
            .for_each(|trigger| trigger.handle(available_requests_count));
    }

    fn handle_all_increasing_triggers(&self) -> Result<()> {
        let requests_difference = self.get_available_requests_in_last_period()?;
        // FIXME what about that? checked_sub on every plase where substitution occured?
        let available_requests_count_on_last_request_time = if requests_difference >= 0 {
            requests_difference
        } else {
            0
        };

        self.more_or_equals_available_requests_count_trigger_scheduler
            .schedule_triggers(
                available_requests_count_on_last_request_time,
                self.get_last_request()?.allowed_start_time,
                self.period_duration,
            );

        Ok(())
    }

    fn get_available_requests_in_last_period(&self) -> Result<usize> {
        let reserved_requests_count = self.get_requests_count_at_last_request_time()?;
        let reserved_requests_counts_without_group = reserved_requests_count.requests_count
            - reserved_requests_count.reserved_in_groups_requests_count;
        let requests_difference = self.requests_per_period
            - reserved_requests_counts_without_group
            - reserved_requests_count.vacant_and_reserved_in_groups_requests_count;

        Ok(requests_difference)
    }

    fn get_requests_count_at_last_request_time(&self) -> Result<RequestsCountsInPeriodResult> {
        let last_request = self.get_last_request()?;
        let last_requests_start_time = last_request.allowed_start_time;
        let period_before_last = last_requests_start_time - self.period_duration;

        let not_period_predicate =
            |request: &Request, _| period_before_last > request.allowed_start_time;

        Ok(self.reserved_requests_count_in_period(period_before_last, not_period_predicate))
    }

    fn get_last_request(&self) -> Result<Request> {
        self.requests
            .last()
            .map(|request| request.clone())
            .ok_or(anyhow!("There are no last request"))
    }

    fn get_available_requests_count_at_persent(&self, current_time: DateTime) -> usize {
        let reserved_requests_count = self.get_reserved_requests_count_at_present(current_time);
        let reserved_requests_counts_without_group = reserved_requests_count.requests_count
            - reserved_requests_count.reserved_in_groups_requests_count;
        let available_requests_count = self.requests_per_period
            - reserved_requests_counts_without_group
            - reserved_requests_count.vacant_and_reserved_in_groups_requests_count;

        available_requests_count
    }

    fn get_reserved_requests_count_at_present(
        &self,
        current_time: DateTime,
    ) -> RequestsCountsInPeriodResult {
        let not_period_predicate = |request: &Request, time| request.allowed_start_time > time;
        self.reserved_requests_count_in_period(current_time, not_period_predicate)
    }

    fn reserved_requests_count_in_period<F>(
        &self,
        current_time: DateTime,
        not_period_predicate: F,
    ) -> RequestsCountsInPeriodResult
    where
        F: Fn(&Request, DateTime) -> bool,
    {
        let pre_reserved_groups = &self.pre_reserved_groups;
        let mut requests_count_by_group_id = HashMap::with_capacity(pre_reserved_groups.len());

        pre_reserved_groups.iter().for_each(|pre_reserved_group| {
            requests_count_by_group_id.insert(
                pre_reserved_group.id,
                RequestsCountTpm::new(pre_reserved_group.pre_reserved_requests_count),
            );
        });

        let mut requests_count = 0;
        let mut requests_count_in_group = 0;
        for request in self.requests.iter() {
            if not_period_predicate(request, current_time) {
                continue;
            }

            requests_count += 1;

            match request.group_id {
                None => continue,
                Some(group_id) => {
                    match requests_count_by_group_id.get_mut(&group_id) {
                        None => {
                            // if some requests from removed groups there are in current period we count its
                            // as requests out of group
                            continue;
                        }
                        Some(requests_count_tmp) => {
                            requests_count_in_group += 1;

                            requests_count_tmp.requests_count += 1;
                        }
                    }
                }
            }
        }
        let mut vacant_and_reserved_count = 0;
        for pair in requests_count_by_group_id {
            let requests_count_tmp = pair.1;
            if requests_count_tmp.requests_count <= requests_count_tmp.pre_reserved_count {
                vacant_and_reserved_count += requests_count_tmp.pre_reserved_count;
            } else {
                vacant_and_reserved_count += requests_count_tmp.requests_count;
            }
        }

        RequestsCountsInPeriodResult::new(
            requests_count,
            requests_count_in_group,
            vacant_and_reserved_count,
        )
    }

    fn get_all_available_requests_count(&self) -> usize {
        let available_requests_number = self.requests_per_period.checked_sub(self.requests.len());

        match available_requests_number {
            Some(available_requests_number) => available_requests_number,
            None => 0,
        }
    }

    fn remove_outdated_requests(&mut self, current_time: DateTime) -> Result<()> {
        let deadline = current_time
            .checked_sub_signed(self.period_duration)
            .ok_or(anyhow!("Unable to substract time periods"))?;
        self.requests
            .retain(|request| request.allowed_start_time >= deadline);

        Ok(())
    }

    fn get_non_decreasing_time(&self, time: DateTime) -> DateTime {
        let last_time = self.last_time;

        match last_time {
            None => time,
            Some(time_value) => {
                if time_value < time {
                    time
                } else {
                    time_value
                }
            }
        }
    }
}

// FIXME Move it somewhere
struct RequestsCountTpm {
    requests_count: usize,
    pre_reserved_count: usize,
}

impl RequestsCountTpm {
    fn new(pre_reserved_count: usize) -> Self {
        Self {
            requests_count: 0,
            pre_reserved_count,
        }
    }
}

#[cfg(test)]
mod test {
    use chrono::Utc;

    use crate::core::exchanges::general::request_type::RequestType;

    use super::*;

    #[test]
    fn some_first_test() {
        let mut requests_timeout_manager = RequestsTimeoutManager::new(
            1,
            Duration::seconds(5),
            ExchangeAccountId::new("test".into(), 0),
        );

        requests_timeout_manager.requests = vec![
            Request::new(RequestType::CreateOrder, Utc::now(), None),
            Request::new(RequestType::CancelOrder, Utc::now(), None),
        ];
        let result = requests_timeout_manager.get_all_available_requests_count();
        dbg!(&result);
    }
}
