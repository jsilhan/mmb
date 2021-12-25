use super::binance::Binance;
use anyhow::Result;
use async_trait::async_trait;
use mmb_core::core::exchanges::common::{ActivePosition, Price};
use mmb_core::core::exchanges::general::symbol::Symbol;
use mmb_core::core::exchanges::rest_client;
use mmb_core::core::exchanges::traits::{ExchangeClient, Support};
use mmb_core::core::orders::order::*;
use mmb_core::core::{
    exchanges::common::{CurrencyPair, RestRequestOutcome},
    orders::pool::OrderRef,
};
use mmb_utils::DateTime;

#[async_trait]
impl ExchangeClient for Binance {
    async fn request_all_symbols(&self) -> Result<RestRequestOutcome> {
        // In current versions works only with Spot market
        let url_path = "/api/v3/exchangeInfo";
        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &vec![])?;

        self.rest_client.get(full_url, &self.settings.api_key).await
    }

    async fn create_order(&self, order: &OrderCreating) -> Result<RestRequestOutcome> {
        let specific_currency_pair = self.get_specific_currency_pair(order.header.currency_pair);

        let mut http_params = vec![
            (
                "symbol".to_owned(),
                specific_currency_pair.as_str().to_owned(),
            ),
            (
                "side".to_owned(),
                Self::to_server_order_side(order.header.side),
            ),
            (
                "type".to_owned(),
                Self::to_server_order_type(order.header.order_type),
            ),
            ("quantity".to_owned(), order.header.amount.to_string()),
            (
                "newClientOrderId".to_owned(),
                order.header.client_order_id.as_str().to_owned(),
            ),
        ];

        if order.header.order_type != OrderType::Market {
            http_params.push(("timeInForce".to_owned(), "GTC".to_owned()));
            http_params.push(("price".to_owned(), order.price.to_string()));
        } else if order.header.execution_type == OrderExecutionType::MakerOnly {
            http_params.push(("timeInForce".to_owned(), "GTX".to_owned()));
        }
        self.add_authentification_headers(&mut http_params)?;

        let url_path = match self.settings.is_margin_trading {
            true => "/fapi/v1/order",
            false => "/api/v3/order",
        };

        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &vec![])?;

        self.rest_client
            .post(full_url, &self.settings.api_key, &http_params)
            .await
    }

    async fn request_cancel_order(&self, order: &OrderCancelling) -> Result<RestRequestOutcome> {
        let specific_currency_pair = self.get_specific_currency_pair(order.header.currency_pair);

        let url_path = match self.settings.is_margin_trading {
            true => "/fapi/v1/order",
            false => "/api/v3/order",
        };

        let mut http_params = vec![
            (
                "symbol".to_owned(),
                specific_currency_pair.as_str().to_owned(),
            ),
            (
                "orderId".to_owned(),
                order.exchange_order_id.as_str().to_owned(),
            ),
        ];
        self.add_authentification_headers(&mut http_params)?;

        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &http_params)?;

        let outcome = self
            .rest_client
            .delete(full_url, &self.settings.api_key)
            .await?;

        Ok(outcome)
    }

    async fn cancel_all_orders(&self, currency_pair: CurrencyPair) -> Result<()> {
        let specific_currency_pair = self.get_specific_currency_pair(currency_pair);

        let host = &self.hosts.rest_host;
        let path_to_delete = "/api/v3/openOrders";

        let mut http_params = vec![(
            "symbol".to_owned(),
            specific_currency_pair.as_str().to_owned(),
        )];
        self.add_authentification_headers(&mut http_params)?;

        let full_url = rest_client::build_uri(host, path_to_delete, &http_params)?;

        let _cancel_order_outcome = self
            .rest_client
            .delete(full_url, &self.settings.api_key)
            .await;

        Ok(())
    }

    async fn request_open_orders(&self) -> Result<RestRequestOutcome> {
        let mut http_params = rest_client::HttpParams::new();
        self.add_authentification_headers(&mut http_params)?;

        self.request_open_orders_by_http_header(http_params).await
    }

    async fn request_open_orders_by_currency_pair(
        &self,
        currency_pair: CurrencyPair,
    ) -> Result<RestRequestOutcome> {
        let specific_currency_pair = self.get_specific_currency_pair(currency_pair);
        let mut http_params = vec![(
            "symbol".to_owned(),
            specific_currency_pair.as_str().to_owned(),
        )];
        self.add_authentification_headers(&mut http_params)?;

        self.request_open_orders_by_http_header(http_params).await
    }

    async fn request_order_info(&self, order: &OrderRef) -> Result<RestRequestOutcome> {
        let specific_currency_pair = self.get_specific_currency_pair(order.currency_pair());

        let url_path = match self.settings.is_margin_trading {
            true => "/fapi/v1/order",
            false => "/api/v3/order",
        };

        let mut http_params = vec![
            (
                "symbol".to_owned(),
                specific_currency_pair.as_str().to_owned(),
            ),
            (
                "origClientOrderId".to_owned(),
                order.client_order_id().as_str().to_owned(),
            ),
        ];
        self.add_authentification_headers(&mut http_params)?;

        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &http_params)?;

        self.rest_client.get(full_url, &self.settings.api_key).await
    }

    async fn request_my_trades(
        &self,
        symbol: &Symbol,
        _last_date_time: Option<DateTime>,
    ) -> Result<RestRequestOutcome> {
        let specific_currency_pair = self.get_specific_currency_pair(symbol.currency_pair());
        let mut http_params = vec![(
            "symbol".to_owned(),
            specific_currency_pair.as_str().to_owned(),
        )];

        self.add_authentification_headers(&mut http_params)?;

        let url_path = match self.settings.is_margin_trading {
            true => "/fapi/v1/userTrades",
            false => "/api/v3/myTrades",
        };

        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &http_params)?;
        self.rest_client.get(full_url, &self.settings.api_key).await
    }

    async fn request_get_position(&self) -> Result<RestRequestOutcome> {
        let mut http_params = Vec::new();
        self.add_authentification_headers(&mut http_params)?;

        let url_path = "/fapi/v2/positionRisk";
        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &http_params)?;

        self.rest_client.get(full_url, &self.settings.api_key).await
    }

    async fn request_get_balance_and_position(&self) -> Result<RestRequestOutcome> {
        panic!("not supported request")
    }

    async fn request_get_balance(&self) -> Result<RestRequestOutcome> {
        let mut http_params = Vec::new();
        self.add_authentification_headers(&mut http_params)?;
        let url_path = match self.settings.is_margin_trading {
            true => "/fapi/v2/account",
            false => "/api/v3/account",
        };

        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &http_params)?;
        self.rest_client.get(full_url, &self.settings.api_key).await
    }

    async fn request_close_position(
        &self,
        position: &ActivePosition,
        price: Option<Price>,
    ) -> Result<RestRequestOutcome> {
        let side = match position.derivative.side {
            Some(side) => side.change_side().to_string(),
            None => "0".to_string(), // unknown side
        };

        let mut http_params = vec![
            (
                "leverage".to_string(),
                position.derivative.leverage.to_string(),
            ),
            ("positionSide".to_string(), "BOTH".to_string()),
            (
                "quantity".to_string(),
                position.derivative.position.abs().to_string(),
            ),
            ("side".to_string(), side),
            (
                "symbol".to_string(),
                position.derivative.currency_pair.to_string(),
            ),
        ];

        match price {
            Some(price) => {
                http_params.push(("type".to_string(), "MARKET".to_string()));
                http_params.push(("price".to_string(), price.to_string()));
            }
            None => http_params.push(("type".to_string(), "LIMIT".to_string())),
        }

        self.add_authentification_headers(&mut http_params)?;

        let url_path = "/fapi/v1/order";
        let full_url = rest_client::build_uri(&self.hosts.rest_host, url_path, &http_params)?;

        self.rest_client
            .post(full_url, &self.settings.api_key, &http_params)
            .await
    }
}