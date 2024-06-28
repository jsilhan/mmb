use anyhow::Result;
use itertools::Itertools;
use mmb_core::balance::manager::balance_manager::BalanceManager;
use mmb_core::disposition_execution::strategy::DispositionStrategy;
use mmb_core::disposition_execution::{
    TradeCycle, TradeDisposition, TradingContext, TradingContextBySide,
};
use mmb_core::explanation::{Explanation, WithExplanation};
use mmb_core::lifecycle::trading_engine::EngineContext;
use mmb_core::order_book::local_snapshot_service::LocalSnapshotsService;
use mmb_core::service_configuration::configuration_descriptor::ConfigurationDescriptor;
use mmb_core::settings::{CurrencyPairSetting, DispositionStrategySettings};
use mmb_domain::events::ExchangeEvent;
use mmb_domain::exchanges::symbol::Round;
use mmb_domain::market::CurrencyPair;
use mmb_domain::market::{ExchangeAccountId, MarketAccountId, MarketId};
use mmb_domain::order::snapshot::{Amount, OrderStatus, Price};
use mmb_domain::order::snapshot::{OrderRole, OrderSide, OrderSnapshot};
use mmb_domain::order_book::local_order_book_snapshot::LocalOrderBookSnapshot;
use mmb_utils::cancellation_token::CancellationToken;
use mmb_utils::infrastructure::WithExpect;
use mmb_utils::DateTime;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PerpSpotArbStrategySettings {
    pub enter_trade_offset: Decimal,
    pub exit_trade_offset: Decimal,
    pub trading_perp_currency_pair: CurrencyPairSetting,
    pub following_spot_currency_pair: CurrencyPairSetting,
    pub max_amount: Decimal,
    pub trading_perp_exchange_account_id: ExchangeAccountId,
    pub following_spot_exchange_account_id: ExchangeAccountId,
    pub diff_history_kept_for_secs: u64,
    pub init_trade_when_min_max_diff_exceeds: Price,
    pub min_max_trade_koeficient: Decimal,
}

#[derive(Debug, Eq, PartialEq)]
pub enum StrategyState {
    ToEnterTrade,
    ToExitTrade(OrderSide),
    End,
}

pub fn clean_currency_pair(currency_pair_settings: &CurrencyPairSetting) -> CurrencyPair {
    if let CurrencyPairSetting::Ordinary { base, quote } = currency_pair_settings {
        CurrencyPair::from_codes(*base, *quote)
    } else {
        panic!(
            "Incorrect currency pair setting enum type {:?}",
            currency_pair_settings
        );
    }
}

impl DispositionStrategySettings for PerpSpotArbStrategySettings {
    fn exchange_account_id(&self) -> ExchangeAccountId {
        self.trading_perp_exchange_account_id
    }

    fn currency_pair(&self) -> CurrencyPair {
        clean_currency_pair(&self.trading_perp_currency_pair)
    }

    // Max amount for orders that will be created
    fn max_amount(&self) -> Amount {
        self.max_amount
    }
}

pub struct PerpSpotArbStrategy {
    trading_perp_exchange_account_id: ExchangeAccountId,
    trading_perp_currency_pair: CurrencyPair,
    following_spot_exchange_account_id: ExchangeAccountId,
    following_spot_currency_pair: CurrencyPair,
    diff_history_kept_for_secs: Duration,
    init_trade_when_min_max_diff_exceeds: Price,
    enter_trade_offset: Decimal,
    exit_trade_offset: Decimal,
    engine_context: Arc<EngineContext>,
    configuration_descriptor: ConfigurationDescriptor,
    max_amount: Decimal,
    diff_prices_interval: Vec<(Instant, Price)>,
    max_diff: Price,
    min_diff: Price,
    state: StrategyState,
}

impl PerpSpotArbStrategy {
    pub fn new(
        trading_perp_exchange_account_id: ExchangeAccountId,
        trading_perp_currency_pair: CurrencyPair,
        following_spot_exchange_account_id: ExchangeAccountId,
        following_spot_currency_pair: CurrencyPair,
        enter_trade_offset: Decimal,
        exit_trade_offset: Decimal,
        max_amount: Decimal,
        diff_history_kept_for_secs: Duration,
        init_trade_when_min_max_diff_exceeds: Price,
        engine_context: Arc<EngineContext>,
    ) -> Box<Self> {
        let configuration_descriptor = ConfigurationDescriptor::new(
            "PerpSpotArbStrategy".into(),
            format!("{trading_perp_exchange_account_id};{trading_perp_currency_pair}")
                .as_str()
                .into(),
        );

        // amount_limit it's a limit for position changing for both sides
        // it's equal to half of the max amount because an order that can change a position from
        // a limit by sells to a limit by buys is possible
        let amount_limit = max_amount * dec!(0.5);

        let symbol = engine_context
            .exchanges
            .get(&trading_perp_exchange_account_id)
            .with_expect(|| format!("failed to get exchange from trading_engine for {trading_perp_exchange_account_id}"))
            .symbols
            .get(&trading_perp_currency_pair)
            .with_expect(|| format!("failed to get symbol from exchange for {trading_perp_currency_pair}"))
            .clone();

        engine_context
            .balance_manager
            .lock()
            .set_target_amount_limit(
                configuration_descriptor,
                trading_perp_exchange_account_id,
                symbol,
                amount_limit,
            );

        Box::new(PerpSpotArbStrategy {
            trading_perp_exchange_account_id,
            trading_perp_currency_pair,
            following_spot_exchange_account_id,
            following_spot_currency_pair,
            diff_history_kept_for_secs,
            init_trade_when_min_max_diff_exceeds,
            enter_trade_offset,
            exit_trade_offset,
            engine_context,
            configuration_descriptor,
            max_amount,
            diff_prices_interval: Vec::new(),
            min_diff: Decimal::MAX,
            max_diff: Decimal::MIN,
            state: StrategyState::ToEnterTrade,
        })
    }

    fn strategy_name() -> &'static str {
        "PerpSpotArbStrategy"
    }

    fn trading_perp_market_account_id(&self) -> MarketAccountId {
        MarketAccountId::new(
            self.trading_perp_exchange_account_id,
            self.trading_perp_currency_pair,
        )
    }

    fn trading_perp_market_id(&self) -> MarketId {
        self.trading_perp_market_account_id().market_id()
    }

    fn following_spot_market_account_id(&self) -> MarketAccountId {
        MarketAccountId::new(
            self.following_spot_exchange_account_id,
            self.following_spot_currency_pair,
        )
    }

    fn following_spot_market_id(&self) -> MarketId {
        self.following_spot_market_account_id().market_id()
    }

    fn update_diff_price_history(&mut self, new_diff_price: Price) -> bool {
        // keeps updated vector of diff prices for DIFF_INTERVAL_DURATION duration
        let now = Instant::now();
        let mut num_diffs_to_remove = 0;
        let mut max_diff_to_remove = Decimal::MIN;
        let mut min_diff_to_remove = Decimal::MAX;

        if let Some((_, diff_price)) = self.diff_prices_interval.last() {
            if *diff_price == new_diff_price {
                return false;
            }
        }

        for (last_diff_time, diff_price) in &self.diff_prices_interval {
            if last_diff_time.elapsed() < self.diff_history_kept_for_secs {
                break;
            };
            max_diff_to_remove = max_diff_to_remove.max(*diff_price);
            min_diff_to_remove = min_diff_to_remove.min(*diff_price);
            num_diffs_to_remove += 1;
        }

        // remove expired diffs
        self.diff_prices_interval.drain(..num_diffs_to_remove);
        if max_diff_to_remove >= self.max_diff {
            self.max_diff = *self
                .diff_prices_interval
                .iter()
                .map(|(_, diff)| diff)
                .max()
                .unwrap_or(&Decimal::MIN)
        }
        if min_diff_to_remove <= self.min_diff {
            self.min_diff = *self
                .diff_prices_interval
                .iter()
                .map(|(_, diff)| diff)
                .min()
                .unwrap_or(&Decimal::MAX)
        }

        // add the new price diff
        self.diff_prices_interval.push((now, new_diff_price));
        self.max_diff = self.max_diff.max(new_diff_price);
        self.min_diff = self.min_diff.min(new_diff_price);

        true
    }

    fn calc_trading_context_by_side(
        &mut self,
        side: OrderSide,
        following_spot_price: &Price,
        trading_perp_snapshot: &LocalOrderBookSnapshot,
        mut explanation: Explanation,
    ) -> Option<TradingContextBySide> {
        let symbol = self
            .engine_context
            .exchanges
            .get(&self.trading_perp_exchange_account_id)?
            .symbols
            .get(&self.trading_perp_currency_pair)?
            .clone();

        let init_trade_when_min_max_diff_exceeds = if self.state == StrategyState::ToEnterTrade {
            self.init_trade_when_min_max_diff_exceeds
        } else {
            dec!(0)
        };
        let price = {
            let (price, rounding) = Self::get_price_for_side(
                side,
                following_spot_price,
                trading_perp_snapshot,
                self.min_diff,
                self.max_diff,
                if self.state == StrategyState::ToEnterTrade {
                    self.enter_trade_offset
                } else {
                    self.exit_trade_offset
                },
                init_trade_when_min_max_diff_exceeds,
            );
            symbol.price_round(price, rounding)
        };

        let amount;
        explanation = {
            let mut explanation = Some(explanation);

            // TODO: delete deep_clone
            let orders = self
                .engine_context
                .exchanges
                .iter()
                .flat_map(|x| {
                    x.orders
                        .not_finished
                        .iter()
                        .map(|y| y.clone())
                        .collect_vec()
                })
                .collect_vec();

            let balance_manager = BalanceManager::clone_and_subtract_not_approved_data(
                self.engine_context.balance_manager.clone(),
                Some(&mut orders.iter()),
            )
            .expect("ExampleStrategy::calc_trading_context_by_side: failed to clone and subtract not approved data for BalanceManager");

            amount = balance_manager
                .lock()
                .get_leveraged_balance_in_amount_currency_code(
                    self.configuration_descriptor,
                    side,
                    self.trading_perp_exchange_account_id,
                    symbol.clone(),
                    price,
                    &mut explanation,
                )
                .with_expect(|| {
                    format!(
                        "Failed to get balance for {}",
                        self.trading_perp_exchange_account_id
                    )
                });

            // This expect can happened if get_leveraged_balance_in_amount_currency_code() sets the explanation to None
            explanation.expect(
                "ExampleStrategy::calc_trading_context_by_side(): Explanation should be non None here"
            )
        };

        let max_amount = self.max_amount;

        Some(TradingContextBySide {
            max_amount: max_amount,
            estimating: vec![WithExplanation {
                value: Some(TradeCycle {
                    order_role: OrderRole::Maker,
                    strategy_name: Self::strategy_name().to_string(),
                    disposition: TradeDisposition::new(
                        self.trading_perp_market_account_id(),
                        side,
                        price,
                        amount,
                    ),
                }),
                explanation,
            }],
        })
    }

    pub fn get_side_to_enter_trade(min_diff: Price, max_diff: Price) -> OrderSide {
        let diff = if max_diff.abs() > min_diff.abs() {
            max_diff
        } else {
            min_diff
        };
        if diff.is_sign_positive() {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        }
    }

    pub fn get_price_for_side(
        side: OrderSide,
        following_spot_price: &Price,
        trading_perp_snapshot: &LocalOrderBookSnapshot,
        min_diff: Price,
        max_diff: Price,
        offset: Decimal,
        init_trade_when_min_max_diff_exceeds: Decimal,
    ) -> (Price, Round) {
        let trading_top_order_book_price = trading_perp_snapshot.get_top(side).unwrap().0;
        if side == OrderSide::Buy {
            let price = (following_spot_price + min_diff - offset)
                .min(
                    following_spot_price + max_diff - init_trade_when_min_max_diff_exceeds - offset,
                )
                .min(trading_top_order_book_price);
            (price, Round::Floor)
        } else {
            let price = (following_spot_price + max_diff + offset)
                .max(
                    following_spot_price + min_diff + init_trade_when_min_max_diff_exceeds + offset,
                )
                .max(trading_top_order_book_price)
                + offset;
            (price, Round::Ceiling)
        }
    }

    fn get_trading_context_to_enter_trade(
        &mut self,
        following_spot_price: &Price,
        trading_perp_snapshot: &LocalOrderBookSnapshot,
        mut explanation: Explanation,
    ) -> Option<TradingContext> {
        let side = Self::get_side_to_enter_trade(self.min_diff, self.max_diff);
        let trading_ctx = self.calc_trading_context_by_side(
            side,
            &following_spot_price,
            trading_perp_snapshot,
            explanation.clone(),
        )?;

        explanation.add_reason(format!("Entering market with just one side {side}"));

        let empty_trading_context_by_side = TradingContextBySide::empty(1, explanation);

        Some(match side {
            OrderSide::Buy => TradingContext::new(trading_ctx, empty_trading_context_by_side),
            OrderSide::Sell => TradingContext::new(empty_trading_context_by_side, trading_ctx),
        })
    }

    fn get_trading_context_to_exit_trade(
        &mut self,
        following_spot_price: &Price,
        trading_perp_snapshot: &LocalOrderBookSnapshot,
        side: OrderSide,
        mut explanation: Explanation,
    ) -> Option<TradingContext> {
        let trading_ctx = self.calc_trading_context_by_side(
            side,
            &following_spot_price,
            trading_perp_snapshot,
            explanation.clone(),
        )?;

        explanation.add_reason(format!("Exiting market with just one side {side}"));

        let empty_trading_context_by_side = TradingContextBySide::empty(1, explanation);

        Some(match side {
            OrderSide::Buy => TradingContext::new(trading_ctx, empty_trading_context_by_side),
            OrderSide::Sell => TradingContext::new(empty_trading_context_by_side, trading_ctx),
        })
    }
}

impl DispositionStrategy for PerpSpotArbStrategy {
    fn calculate_trading_context(
        &mut self,
        _event: &ExchangeEvent,
        _now: DateTime,
        local_snapshots_service: &LocalSnapshotsService,
        explanation: &mut Explanation,
    ) -> Option<TradingContext> {
        let trading_perp_snapshot =
            local_snapshots_service.get_snapshot(self.trading_perp_market_id())?;
        let following_spot_snapshot =
            local_snapshots_service.get_snapshot(self.following_spot_market_id())?;

        let following_spot_price =
            following_spot_snapshot.calculate_global_average_symmetric_price()?;
        let new_diff_price = trading_perp_snapshot.get_top_ask()?.0 - following_spot_price;
        self.update_diff_price_history(new_diff_price.round_dp(1));

        let state = &self.state;
        match state {
            StrategyState::ToEnterTrade => self.get_trading_context_to_enter_trade(
                &following_spot_price,
                trading_perp_snapshot,
                explanation.clone(),
            ),
            StrategyState::ToExitTrade(side) => self.get_trading_context_to_exit_trade(
                &following_spot_price,
                trading_perp_snapshot,
                *side,
                explanation.clone(),
            ),
            StrategyState::End => {
                let empty_trading_context_by_side =
                    TradingContextBySide::empty(1, explanation.clone());
                Some(TradingContext::new(
                    empty_trading_context_by_side.clone(),
                    empty_trading_context_by_side,
                ))
            }
        }
    }

    fn handle_order_fill(
        &mut self,
        filled_order: &Arc<OrderSnapshot>,
        _target_eai: ExchangeAccountId,
        _cancellation_token: CancellationToken,
    ) -> Result<()> {
        // currently we handle partial and full order fills the same
        // and keep partially filled orders opened
        // while creating a limit order on another side with the full amount

        if filled_order.props.status != OrderStatus::Completed {
            // the first partial order fill was already handled, we don't want to switch states
            return Ok(());
        }

        let state = &self.state;
        self.state = match state {
            StrategyState::ToEnterTrade => {
                StrategyState::ToExitTrade(filled_order.side().change_side())
            }
            StrategyState::ToExitTrade(_) => StrategyState::ToEnterTrade,
            StrategyState::End => StrategyState::End,
        };
        Ok(())
    }

    fn configuration_descriptor(&self) -> ConfigurationDescriptor {
        self.configuration_descriptor
    }
}
