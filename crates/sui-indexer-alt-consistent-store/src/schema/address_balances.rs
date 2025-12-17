// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use bincode::{Decode, Encode};
use move_core_types::language_storage::TypeTag;
use sui_indexer_alt_framework::types::base_types::SuiAddress;

#[derive(Encode, Decode, PartialEq, Eq, Ord, PartialOrd)]
pub(crate) struct Key {
    #[bincode(with_serde)]
    pub(crate) owner: SuiAddress,

    #[bincode(with_serde)]
    pub(crate) type_: TypeTag,
}

/// Options for creating this index's column family in RocksDB.
pub(crate) fn options(base_options: &rocksdb::Options) -> rocksdb::Options {
    base_options.clone()
}
