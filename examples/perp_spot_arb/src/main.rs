#![deny(
    non_ascii_idents,
    non_shorthand_field_patterns,
    no_mangle_generic_items,
    overflowing_literals,
    path_statements,
    unused_allocation,
    unused_comparisons,
    unused_parens,
    while_true,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    unused_must_use,
    clippy::unwrap_used
)]

mod orders_activity;

use anyhow::Result;
use binance::binance::BinanceBuilder;
use bitmex::bitmex::BitmexBuilder;
use mmb_core::config::{CONFIG_PATH, CREDENTIALS_PATH};
use mmb_core::infrastructure::{spawn_future, spawn_future_ok};
use mmb_core::lifecycle::app_lifetime_manager::ActionAfterGracefulShutdown;
use mmb_core::lifecycle::launcher::{launch_trading_engine, EngineBuildConfig, InitSettings};
use mmb_core::settings::DispositionStrategySettings;
use mmb_utils::infrastructure::SpawnFutureFlags;
use strategies::perp_spot_arb_strategy::{
    clean_currency_pair, PerpSpotArbStrategy, PerpSpotArbStrategySettings,
};
use vis_robot_integration::start_visualization_data_saving;

const STRATEGY_NAME: &str = "perp_spot_arb";

#[tokio::main]
async fn main() -> Result<()> {
    let engine_config =
        EngineBuildConfig::new(vec![Box::new(BitmexBuilder), Box::new(BinanceBuilder)]);

    let init_settings = InitSettings::<PerpSpotArbStrategySettings>::Load {
        config_path: CONFIG_PATH.to_owned(),
        credentials_path: CREDENTIALS_PATH.to_owned(),
    };
    loop {
        let engine = launch_trading_engine(&engine_config, init_settings.clone()).await?;

        let ctx = engine.context();
        let settings = engine.settings();

        spawn_future(
            "Save visualization data",
            SpawnFutureFlags::STOP_BY_TOKEN | SpawnFutureFlags::DENY_CANCELLATION,
            start_visualization_data_saving(ctx.clone(), STRATEGY_NAME),
        );

        spawn_future_ok(
            "Checking orders activity",
            SpawnFutureFlags::STOP_BY_TOKEN | SpawnFutureFlags::DENY_CANCELLATION,
            orders_activity::checking_orders_activity(ctx.clone()),
        );

        let strategy = PerpSpotArbStrategy::new(
            settings.strategy.exchange_account_id(),
            settings.strategy.currency_pair(),
            settings.strategy.following_spot_exchange_account_id,
            clean_currency_pair(&settings.strategy.following_spot_currency_pair),
            settings.strategy.enter_trade_offset,
            settings.strategy.exit_trade_offset,
            settings.strategy.max_amount,
            std::time::Duration::from_secs(settings.strategy.diff_history_kept_for_secs),
            settings.strategy.init_trade_when_min_max_diff_exceeds,
            ctx.clone(),
        );

        engine.start_disposition_executor(strategy);

        match engine.run().await {
            ActionAfterGracefulShutdown::Nothing => break,
            ActionAfterGracefulShutdown::Restart => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mmb_domain::{
        exchanges::symbol::Round,
        order::snapshot::{OrderSide, Price, SortedOrderData},
        order_book::local_order_book_snapshot::LocalOrderBookSnapshot,
    };
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use strategies::perp_spot_arb_strategy::PerpSpotArbStrategy;

    use chrono::Utc;
    use test_case::test_case;

    #[test_case(dec!(10), dec!(30), dec!(0), dec!(10), dec!(3000), dec!(3030), OrderSide::Sell)]
    #[test_case(dec!(10), dec!(30), dec!(2), dec!(10), dec!(3000), dec!(3032), OrderSide::Sell)]
    #[test_case(dec!(20), dec!(40), dec!(0), dec!(10), dec!(3000), dec!(3040),OrderSide::Sell)]
    #[test_case(dec!(10), dec!(20), dec!(0), dec!(0), dec!(3000), dec!(3021), OrderSide::Sell)]
    #[test_case(dec!(20), dec!(30), dec!(0), dec!(20), dec!(3000), dec!(3040),OrderSide::Sell)]
    #[test_case(dec!(-30), dec!(-10), dec!(0), dec!(10), dec!(3040), dec!(3010), OrderSide::Buy)]
    #[test_case(dec!(-30), dec!(-10), dec!(2), dec!(10), dec!(3040), dec!(3008), OrderSide::Buy)]
    #[test_case(dec!(-40), dec!(-30), dec!(0), dec!(10), dec!(3040), dec!(3000), OrderSide::Buy)]
    #[test_case(dec!(-20), dec!(-10), dec!(0), dec!(0), dec!(3040), dec!(3020), OrderSide::Buy)]
    #[test_case(dec!(-30), dec!(-20), dec!(0), dec!(20), dec!(3040), dec!(3000), OrderSide::Buy)]
    fn get_price_for_side(
        min_diff: Price,
        max_diff: Price,
        offset: Decimal,
        init_trade_when_min_max_diff_exceeds: Decimal,
        following_spot_price: Decimal,
        result_price: Price,
        result_side: OrderSide,
    ) {
        let mut asks = SortedOrderData::new();
        asks.insert(dec!(3021), dec!(1));
        let mut bids = SortedOrderData::new();
        bids.insert(dec!(3020), dec!(1));
        let trading_perp_snapshot = LocalOrderBookSnapshot::new(asks, bids, Utc::now());

        let side = PerpSpotArbStrategy::get_side_to_enter_trade(min_diff, max_diff);
        assert_eq!(side, result_side);

        let (price, rounding) = PerpSpotArbStrategy::get_price_for_side(
            side,
            &following_spot_price,
            &trading_perp_snapshot,
            min_diff,
            max_diff,
            offset,
            init_trade_when_min_max_diff_exceeds,
        );
        assert_eq!(price, result_price);
        let result_rounding = match side {
            OrderSide::Sell => Round::Ceiling,
            OrderSide::Buy => Round::Floor,
        };
        assert_eq!(rounding, result_rounding);
    }
}
