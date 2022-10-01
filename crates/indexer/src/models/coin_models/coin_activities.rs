// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

// This is required because a diesel macro makes clippy sad
#![allow(clippy::extra_unused_lifetimes)]
#![allow(clippy::unused_unit)]

use super::{
    coin_balances::{CoinBalance, CurrentCoinBalance},
    coin_infos::{CoinInfo, CoinSupplyLookup},
    coin_utils::{CoinEvent, EventGuidResource},
};
use crate::{schema::coin_activities, util::truncate_str};
use aptos_api_types::{
    Event as APIEvent, Transaction as APITransaction, TransactionInfo as APITransactionInfo,
    TransactionPayload, UserTransactionRequest, WriteSetChange as APIWriteSetChange,
};
use aptos_types::APTOS_COIN_TYPE;
use bigdecimal::BigDecimal;
use field_count::FieldCount;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const BURN_GAS_EVENT: &str = "0x1::aptos_coin::GasBurnEvent";
// We will never have a negative number on chain so this will avoid collision in postgres
const BURN_GAS_EVENT_CREATION_NUM: i64 = -1;
// We will never have a negative number on chain so this will avoid collision in postgres
const BURN_GAS_EVENT_SEQUENCE_NUM: i64 = -1;

type OwnerAddress = String;
type CoinType = String;
// Primary key of the current_coin_balances table, i.e. (owner_address, coin_type)
pub type CurrentCoinBalancePK = (OwnerAddress, CoinType);
pub type EventToCoinType = HashMap<EventGuidResource, CoinType>;

#[derive(Debug, Deserialize, FieldCount, Identifiable, Insertable, Queryable, Serialize)]
#[diesel(primary_key(
    transaction_version,
    event_account_address,
    event_creation_number,
    event_sequence_number
))]
#[diesel(table_name = coin_activities)]
pub struct CoinActivity {
    pub transaction_version: i64,
    pub event_account_address: String,
    pub event_creation_number: i64,
    pub event_sequence_number: i64,
    pub owner_address: String,
    pub coin_type: String,
    pub amount: BigDecimal,
    pub activity_type: String,
    pub is_gas_fee: bool,
    pub is_transaction_success: bool,
    pub entry_function_id_str: Option<String>,
    pub inserted_at: chrono::NaiveDateTime,
}

/// Coin information is mostly in Resources but some pieces are in table items (e.g. supply from aggregator table)
pub struct CoinSupply {
    pub coin_type: String,
    pub transaction_version_created: i64,
    pub creator_address: String,
    pub name: String,
    pub symbol: String,
    pub decimals: i32,
    pub supply: BigDecimal,
}

impl CoinActivity {
    /// There are different objects containing different information about balances and coins.
    /// Events: Withdraw and Deposit event containing amounts. There is no coin type so we need to get that from Resources. (from event guid)
    /// CoinInfo Resource: Contains name, symbol, decimals and supply. (if supply is aggregator, however, actual supply amount will live in a separate table)
    /// CoinStore Resource: Contains owner address and coin type information used to complete events
    /// Aggregator Table Item: Contains current supply of a coin
    pub fn from_transaction(
        transaction: &APITransaction,
    ) -> (
        Vec<Self>,
        Vec<CoinInfo>,
        Vec<CoinBalance>,
        HashMap<CurrentCoinBalancePK, CurrentCoinBalance>,
    ) {
        let mut coin_activities = Vec::new();
        let mut coin_infos = Vec::new();
        let mut coin_balances = Vec::new();
        let mut current_coin_balances: HashMap<CurrentCoinBalancePK, CurrentCoinBalance> =
            HashMap::new();
        let mut all_event_to_coin_type: EventToCoinType = HashMap::new();

        let (txn_info, writesets, events, maybe_user_request) = match &transaction {
            APITransaction::GenesisTransaction(inner) => {
                (&inner.info, &inner.info.changes, &inner.events, None)
            }
            APITransaction::UserTransaction(inner) => (
                &inner.info,
                &inner.info.changes,
                &inner.events,
                Some(&inner.request),
            ),
            _ => return Default::default(),
        };

        let mut entry_function_id_str = None;
        if let Some(user_request) = maybe_user_request {
            entry_function_id_str = match &user_request.payload {
                TransactionPayload::EntryFunctionPayload(payload) => {
                    Some(truncate_str(&payload.function.to_string(), 100))
                }
                _ => None,
            };
            coin_activities.push(Self::get_gas_event(txn_info, user_request));
        }
        // First we need to make a pass to get all tables that potentially contains coin supply information
        let mut supply_lookup: CoinSupplyLookup = HashMap::new();
        for wsc in writesets {
            if let APIWriteSetChange::WriteTableItem(table_item) = &wsc {
                let item = CoinInfo::get_aggregator_supply_lookup(table_item).unwrap();
                supply_lookup.extend(item);
            }
        }

        // Get coin info, then coin balances. We can leverage coin balances to get the metadata required for events
        let txn_version = txn_info.version.0 as i64;
        for wsc in writesets {
            let (maybe_coin_info, maybe_coin_balance_data) =
                if let APIWriteSetChange::WriteResource(write_resource) = wsc {
                    (
                        CoinInfo::from_write_resource(write_resource, txn_version, &supply_lookup)
                            .unwrap(),
                        CoinBalance::from_write_resource(write_resource, txn_version).unwrap(),
                    )
                } else {
                    (None, None)
                };
            if let Some(coin_info) = maybe_coin_info {
                coin_infos.push(coin_info);
            }
            if let Some((coin_balance, current_coin_balance, event_to_coin_type)) =
                maybe_coin_balance_data
            {
                current_coin_balances.insert(
                    (
                        coin_balance.owner_address.clone(),
                        coin_balance.coin_type.clone(),
                    ),
                    current_coin_balance,
                );
                coin_balances.push(coin_balance);
                all_event_to_coin_type.extend(event_to_coin_type);
            }
        }
        for event in events {
            let event_type = event.typ.to_string();
            match CoinEvent::from_event(event_type.as_str(), &event.data, txn_version).unwrap() {
                Some(parsed_event) => coin_activities.push(Self::from_parsed_event(
                    &event_type,
                    event,
                    &parsed_event,
                    txn_version,
                    &all_event_to_coin_type,
                    &entry_function_id_str,
                )),
                None => {}
            };
        }
        (
            coin_activities,
            coin_infos,
            coin_balances,
            current_coin_balances,
        )
    }

    fn from_parsed_event(
        event_type: &str,
        event: &APIEvent,
        coin_event: &CoinEvent,
        txn_version: i64,
        event_to_coin_type: &EventToCoinType,
        entry_function_id_str: &Option<String>,
    ) -> Self {
        let amount = match coin_event {
            CoinEvent::WithdrawCoinEvent(inner) => inner.amount.clone(),
            CoinEvent::DepositCoinEvent(inner) => inner.amount.clone(),
        };
        let event_move_guid = EventGuidResource {
            addr: event.guid.account_address.to_string(),
            creation_num: event.guid.creation_number.0 as i64,
        };
        let coin_type =
            event_to_coin_type
                .get(&event_move_guid)
                .unwrap_or_else(|| {
                    panic!(
                        "Could not find event in resources (CoinStore), version: {}, event guid: {:?}, mapping: {:?}",
                        txn_version, event_move_guid, event_to_coin_type
                    )
                }).clone();

        Self {
            transaction_version: txn_version,
            event_account_address: event.guid.account_address.to_string(),
            event_creation_number: event.guid.creation_number.0 as i64,
            event_sequence_number: event.sequence_number.0 as i64,
            owner_address: event.guid.account_address.to_string(),
            coin_type,
            amount,
            activity_type: event_type.to_string(),
            is_gas_fee: false,
            is_transaction_success: true,
            entry_function_id_str: entry_function_id_str.clone(),
            inserted_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn get_gas_event(
        txn_info: &APITransactionInfo,
        user_transaction_request: &UserTransactionRequest,
    ) -> Self {
        let aptos_coin_burned = BigDecimal::from(
            txn_info.gas_used.0 * user_transaction_request.gas_unit_price.0 as u64,
        );

        Self {
            transaction_version: txn_info.version.0 as i64,
            event_account_address: user_transaction_request.sender.to_string(),
            event_creation_number: BURN_GAS_EVENT_CREATION_NUM,
            event_sequence_number: BURN_GAS_EVENT_SEQUENCE_NUM,
            owner_address: user_transaction_request.sender.to_string(),
            coin_type: APTOS_COIN_TYPE.to_string(),
            amount: aptos_coin_burned,
            activity_type: BURN_GAS_EVENT.to_string(),
            is_gas_fee: true,
            is_transaction_success: txn_info.success,
            entry_function_id_str: None,
            inserted_at: chrono::Utc::now().naive_utc(),
        }
    }
}
