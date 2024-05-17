use async_trait::async_trait;

use super::profit_loss_balance_change::ProfitLossBalanceChange;

#[async_trait]
pub(crate) trait BalanceChangeAccumulator {
    fn add_balance_change(&self, balance_change: &ProfitLossBalanceChange);
}
