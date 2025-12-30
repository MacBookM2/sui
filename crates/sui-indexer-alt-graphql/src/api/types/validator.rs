// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use async_graphql::{Context, Object, connection::Connection};

use crate::{
    api::{
        scalars::{cursor::JsonCursor, uint53::UInt53},
        types::{
            move_type::MoveType,
            move_value::MoveValue,
            validator_set::{ValidatorContents, ValidatorSetContents},
        },
    },
    error::RpcError,
    pagination::{Page, PaginationConfig},
};

#[derive(Clone)]
pub(crate) struct Validator {
    pub(crate) contents: Arc<ValidatorSetContents>,
    pub(crate) idx: usize,
}

type CAddr = JsonCursor<usize>;

#[Object]
impl Validator {
    /// The number of epochs for which this validator has been below the low stake threshold.
    async fn at_risk(&self) -> Option<UInt53> {
        let validator = self.validator()?;
        Some(UInt53::from(validator.at_risk))
    }

    /// On-chain representation of the underlying `0x3::validator::Validator` value.
    async fn contents(&self) -> Option<MoveValue> {
        let validator = self.validator()?;
        let native = validator.bytes.clone();

        let type_ = MoveType::from_native(
            ValidatorContents::tag().into(),
            self.contents.scope().clone(),
        );

        Some(MoveValue { type_, native })
    }

    /// Other validators this validator has reported.
    async fn report_records(
        &self,
        ctx: &Context<'_>,
        first: Option<u64>,
        before: Option<CAddr>,
        last: Option<u64>,
        after: Option<CAddr>,
    ) -> Result<Option<Connection<String, Validator>>, RpcError> {
        let Some(validator) = self.validator() else {
            return Ok(None);
        };

        let pagination: &PaginationConfig = ctx.data()?;
        let limits = pagination.limits("Validator", "reportRecords");
        let page = Page::from_params(limits, first, after, last, before)?;
        page.paginate_indices(validator.reports.len(), |i| {
            let idx = validator.reports[i];
            Ok(Validator {
                contents: Arc::clone(&self.contents),
                idx,
            })
        })
        .map(Some)
    }
}

impl Validator {
    fn validator(&self) -> Option<&ValidatorContents> {
        self.contents.active_validators.get(self.idx)
    }
}
