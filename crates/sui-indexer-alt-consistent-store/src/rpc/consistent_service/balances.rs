// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::cmp::Ordering;
use std::str::FromStr;

use anyhow::Context;
use bincode::serde::Compat;
use sui_indexer_alt_consistent_api::proto::rpc::consistent::v1alpha as grpc;
use sui_indexer_alt_framework::types::{TypeTag, base_types::SuiAddress};

use crate::{
    rpc::{
        error::{RpcError, StatusCode, db_error},
        pagination::Page,
    },
    schema::address_balances::Key as AddressKey,
    schema::balances::Key as CoinKey,
};

use super::State;

#[derive(thiserror::Error, Debug)]
pub(super) enum Error {
    #[error("Invalid 'owner': {0:?}")]
    InvalidOwner(String),

    #[error("Invalid 'coin_type': {0:?}")]
    InvalidType(String),

    #[error("Missing 'owner'")]
    MissingOwner,

    #[error("Missing 'coin_type'")]
    MissingType,

    #[error("Too many requests in batch: {0} (max: {1})")]
    TooManyRequests(usize, u32),
}

impl StatusCode for Error {
    fn code(&self) -> tonic::Code {
        match self {
            Error::InvalidOwner(_)
            | Error::InvalidType(_)
            | Error::MissingOwner
            | Error::MissingType
            | Error::TooManyRequests(_, _) => tonic::Code::InvalidArgument,
        }
    }
}

pub(super) fn batch_get_balances(
    state: &State,
    checkpoint: u64,
    request: grpc::BatchGetBalancesRequest,
) -> Result<grpc::BatchGetBalancesResponse, RpcError<Error>> {
    let config = &state.rpc_config.pagination;
    let raw_keys = if request.requests.len() > config.max_batch_size as usize {
        return Err(Error::TooManyRequests(
            request.requests.len(),
            state.rpc_config.pagination.max_batch_size,
        )
        .into());
    } else {
        request
            .requests
            .into_iter()
            .map(key)
            .collect::<Result<Vec<_>, _>>()?
    };

    let cb_keys: Vec<CoinKey> = raw_keys
        .iter()
        .map(|(owner, type_)| CoinKey {
            owner: *owner,
            type_: type_.clone(),
        })
        .collect();

    let index = &state.store.schema().balances;
    let cbs = index
        .multi_get(checkpoint, &cb_keys)
        .map_err(|e| db_error(e, "failed to batch get balances"))?;

    let ab_keys: Vec<AddressKey> = raw_keys
        .iter()
        .map(|(owner, type_)| AddressKey {
            owner: *owner,
            type_: type_.clone(),
        })
        .collect();

    let index = &state.store.schema().address_balances;
    let abs = index
        .multi_get(checkpoint, &ab_keys)
        .map_err(|e| db_error(e, "failed to batch get address balances"))?;

    let mut balances = Vec::with_capacity(raw_keys.len());
    for ((cb, ab), key) in cbs.into_iter().zip(abs).zip(raw_keys) {
        let with_prefix = true;
        let coin_type = key.1.to_canonical_string(with_prefix);
        let coin_balance = cb.context("Failed to deserialize balance")?.unwrap_or(0);
        let coin_balance = u64::try_from(coin_balance)
            .with_context(|| format!("Bad balance for type {coin_type}: {coin_balance}"))?;
        let address_balance = ab
            .context("Failed to deserialize address balance")?
            .unwrap_or(0);
        let address_balance = u64::try_from(address_balance).with_context(|| {
            format!("Bad address balance for type {coin_type}: {address_balance}")
        })?;

        balances.push(grpc::Balance {
            owner: Some(key.0.to_string()),
            coin_type: Some(coin_type),
            total_balance: Some(address_balance + coin_balance),
            address_balance: Some(address_balance),
            coin_balance: Some(coin_balance),
            page_token: None,
        });
    }

    Ok(grpc::BatchGetBalancesResponse { balances })
}

pub(super) fn get_balance(
    state: &State,
    checkpoint: u64,
    request: grpc::GetBalanceRequest,
) -> Result<grpc::Balance, RpcError<Error>> {
    let (owner, type_) = key(request)?;
    let key = CoinKey { owner, type_ };
    let index = &state.store.schema().balances;
    let coin_balance = index
        .get(checkpoint, &key)
        .map_err(|e| db_error(e, "failed to get coin balance"))?
        .unwrap_or(0);

    let with_prefix = true;
    let coin_type = key.type_.to_canonical_string(with_prefix);
    let coin_balance = u64::try_from(coin_balance)
        .with_context(|| format!("Bad balance for type {coin_type}: {coin_balance}"))?;

    let ab_key = AddressKey {
        owner: key.owner,
        type_: key.type_.clone(),
    };
    let index = &state.store.schema().address_balances;
    let address_balance = index
        .get(checkpoint, &ab_key)
        .map_err(|e| db_error(e, "failed to get address balance"))?
        .unwrap_or(0);
    let address_balance = u64::try_from(address_balance)
        .with_context(|| format!("Bad address balance for type {coin_type}: {address_balance}"))?;
    println!("Address balance: {}", address_balance);
    println!("Coin balance: {}", coin_balance);
    println!("Total balance: {}", address_balance + coin_balance);

    Ok(grpc::Balance {
        owner: Some(key.owner.to_string()),
        coin_type: Some(coin_type),
        total_balance: Some(address_balance + coin_balance),
        address_balance: Some(address_balance),
        coin_balance: Some(coin_balance),
        page_token: None,
    })
}

pub(super) fn list_balances(
    state: &State,
    checkpoint: u64,
    request: grpc::ListBalancesRequest,
) -> Result<grpc::ListBalancesResponse, RpcError<Error>> {
    let owner = if request.owner().is_empty() {
        return Err(Error::MissingOwner.into());
    } else {
        owner(request.owner())?
    };

    let page = Page::from_request(
        &state.rpc_config.pagination,
        request.after_token(),
        request.before_token(),
        request.page_size(),
        request.end(),
    );

    let index = &state.store.schema().balances;
    // Zero balances may end up in the databases through accumulation. They will eventually be
    // cleaned up by compaction, but until then, they need to be filtered out of results (similar
    // to how RocksDB filters out tombstones).
    let cb_resp = page.paginate_filtered(index, checkpoint, &Compat(owner), |_, _, balance| {
        *balance > 0
    })?;

    let index = &state.store.schema().address_balances;
    let ab_resp = page.paginate_prefix(index, checkpoint, &Compat(owner))?;

    let merged_results = merge_balances(cb_resp.results, ab_resp.results);
    let limit = page.limit();
    let has_overflow = merged_results.len() > limit;
    let (has_prev, has_next) = if page.is_from_front() {
        // Forward: overflow affects has_next
        (
            cb_resp.has_prev || ab_resp.has_prev,
            has_overflow || cb_resp.has_next || ab_resp.has_next,
        )
    } else {
        // Backward: overflow affects has_prev
        (
            has_overflow || cb_resp.has_prev || ab_resp.has_prev,
            cb_resp.has_next || ab_resp.has_next,
        )
    };

    let truncated: Vec<_> = if page.is_from_front() {
        merged_results.into_iter().take(limit).collect()
    } else {
        let skip = merged_results.len().saturating_sub(limit);
        merged_results.into_iter().skip(skip).collect()
    };

    let mut balances = vec![];
    for (token, key, coin_balance, address_balance) in truncated {
        let coin_type = key.1.to_canonical_string(/* with_prefix */ true);
        let coin_balance = u64::try_from(coin_balance)
            .with_context(|| format!("Bad balance for type {coin_type}: {coin_balance}"))?;

        let address_balance = u64::try_from(address_balance).with_context(|| {
            format!("Bad address balance for type {coin_type}: {address_balance}")
        })?;

        balances.push(grpc::Balance {
            owner: Some(key.0.to_string()),
            coin_type: Some(coin_type),
            total_balance: Some(address_balance + coin_balance),
            address_balance: Some(address_balance),
            coin_balance: Some(coin_balance),
            page_token: Some(token.into()),
        });
    }

    Ok(grpc::ListBalancesResponse {
        has_previous_page: Some(has_prev),
        has_next_page: Some(has_next),
        balances,
    })
}

/// Merge coin and address balances for the same owner on coin type. The inputs are expected to be
/// in ascending order.
fn merge_balances(
    coin_balances: Vec<(Vec<u8>, CoinKey, i128)>,
    address_balances: Vec<(Vec<u8>, AddressKey, u128)>,
) -> Vec<(Vec<u8>, (SuiAddress, TypeTag), i128, u128)> {
    let mut merged = Vec::with_capacity(coin_balances.len() + address_balances.len());
    let mut coins = coin_balances.into_iter().peekable();
    let mut addresses = address_balances.into_iter().peekable();
    loop {
        let pick_from = match (coins.peek(), addresses.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((_, ck, _)), Some((_, ak, _))) => ck.type_.cmp(&ak.type_),
        };

        let next = match pick_from {
            Ordering::Less => {
                let (token, key, balance) = coins.next().unwrap();
                Some((token, (key.owner, key.type_.clone()), balance, 0))
            }
            Ordering::Greater => {
                let (token, key, balance) = addresses.next().unwrap();
                Some((token, (key.owner, key.type_.clone()), 0, balance))
            }
            Ordering::Equal => {
                let (ctoken, ckey, cbalance) = coins.next().unwrap();
                let (_, _, abalance) = addresses.next().unwrap();
                Some((ctoken, (ckey.owner, ckey.type_.clone()), cbalance, abalance))
            }
        };

        if let Some(item) = next {
            merged.push(item);
        }
    }
    merged
}

/// Convert a point lookup into a key for the underlying index.
fn key(request: grpc::GetBalanceRequest) -> Result<(SuiAddress, TypeTag), Error> {
    let owner = if request.owner().is_empty() {
        return Err(Error::MissingOwner);
    } else {
        owner(request.owner())?
    };

    let type_ = if request.coin_type().is_empty() {
        return Err(Error::MissingType);
    } else {
        TypeTag::from_str(request.coin_type())
            .map_err(|_| Error::InvalidType(request.coin_type().to_owned()))?
    };

    Ok((owner, type_))
}

/// Parse the owner's `SuiAddress` from a string. Addresses must start with `0x` followed by
/// between 1 and 64 hexadecimal characters.
///
/// TODO: Switch to using `sui_sdk_types::Address`, once the indexing framework is ported to the
/// new SDK.
fn owner(input: &str) -> Result<SuiAddress, Error> {
    let Some(s) = input.strip_prefix("0x") else {
        return Err(Error::InvalidOwner(input.to_owned()));
    };

    let s = if s.is_empty() || s.len() > 64 {
        return Err(Error::InvalidOwner(s.to_owned()));
    } else if s.len() != 64 {
        Cow::Owned(format!("0x{s:0>64}"))
    } else {
        Cow::Borrowed(input)
    };

    SuiAddress::from_str(s.as_ref()).map_err(|_| Error::InvalidOwner(s.to_string()))
}
