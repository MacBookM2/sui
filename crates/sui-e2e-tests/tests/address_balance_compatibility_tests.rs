// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use sui_json_rpc_api::CoinReadApiClient;
use sui_json_rpc_types::SuiTransactionBlockEffectsAPI;
use sui_json_rpc_types::SuiTransactionBlockResponse;
use sui_keys::keystore::AccountKeystore;
use sui_macros::*;
use sui_protocol_config::ProtocolConfig;
use sui_sdk::wallet_context::WalletContext;
use sui_test_transaction_builder::{FundSource, TestTransactionBuilder};
use sui_types::{
    accumulator_root::AccumulatorValue,
    balance::Balance,
    base_types::{FullObjectRef, ObjectID, ObjectRef, SequenceNumber, SuiAddress},
    coin_reservation::ParsedObjectRefWithdrawal,
    effects::TransactionEffectsAPI,
};
use test_cluster::{TestCluster, TestClusterBuilder};

async fn get_sender_and_all_gas(context: &mut WalletContext) -> (SuiAddress, Vec<ObjectRef>) {
    get_nth_sender_and_all_gas(context, 0).await
}

async fn get_sender_and_one_gas(context: &mut WalletContext) -> (SuiAddress, ObjectRef) {
    let (sender, gas) = get_sender_and_all_gas(context).await;
    (sender, gas.into_iter().next().unwrap())
}

async fn get_nth_sender_and_one_gas(
    context: &mut WalletContext,
    n: usize,
) -> (SuiAddress, ObjectRef) {
    let (sender, gas) = get_nth_sender_and_all_gas(context, n).await;
    (sender, gas.into_iter().next().unwrap())
}

async fn get_nth_sender_and_all_gas(
    context: &mut WalletContext,
    n: usize,
) -> (SuiAddress, Vec<ObjectRef>) {
    let sender = context
        .config
        .keystore
        .addresses()
        .into_iter()
        .nth(n)
        .unwrap();

    let gas = context
        .gas_objects(sender)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, obj)| obj.object_ref())
        .collect();

    (sender, gas)
}

#[sim_test]
async fn test_coin_reservation_validation() {
    let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut cfg| {
        cfg.create_root_accumulator_object_for_testing();
        cfg.enable_accumulators_for_testing();
        cfg.enable_coin_reservation_for_testing();
        cfg
    });

    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    let rgp = test_cluster.get_reference_gas_price().await;
    let chain_id = test_cluster.get_chain_identifier();
    let context = &mut test_cluster.wallet;

    let (sender1, gas1) = get_nth_sender_and_one_gas(context, 0).await;
    let (sender2, gas2) = get_nth_sender_and_one_gas(context, 1).await;

    // send 1000 gas from the gas coins to the balances
    let tx = TestTransactionBuilder::new(sender1, gas1, rgp)
        .transfer_sui_to_address_balance(FundSource::coin(gas1), vec![(1000, sender1)])
        .build();

    let (_, effects) = test_cluster
        .sign_and_execute_transaction_directly(&tx)
        .await
        .unwrap();
    let gas1 = effects.gas_object().0;

    // compute the sender's SUI accumulator object id
    let accumulator_obj_id = AccumulatorValue::get_field_id(
        sender1,
        &Balance::type_tag(sui_types::gas_coin::GAS::type_tag()),
    )
    .unwrap();

    let encode_coin_reservation = |epoch: u64, amount: u64| {
        ParsedObjectRefWithdrawal::new(*accumulator_obj_id.inner(), epoch, amount)
            .encode(SequenceNumber::new(), chain_id)
    };

    // Verify transaction is rejected if it reserves more than the available balance
    {
        let coin_reservation = encode_coin_reservation(0, 1001);

        let err =
            try_coin_reservation_tx(&mut test_cluster, coin_reservation, sender1, sender1, gas1)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("is less than requested"));
    }

    // Verify transaction is rejected if it uses a bogus accumulator object id.
    {
        let random_id = ObjectID::random();
        let coin_reservation = ParsedObjectRefWithdrawal::new(random_id, 0, 1001)
            .encode(SequenceNumber::new(), chain_id);

        let err =
            try_coin_reservation_tx(&mut test_cluster, coin_reservation, sender1, sender1, gas1)
                .await
                .unwrap_err();
        assert!(
            err.to_string()
                .contains(format!("object id {} not found", random_id).as_str())
        );
    }

    // Verify transaction is rejected if it is not valid in the current epoch.
    {
        let coin_reservation = encode_coin_reservation(1, 100);

        let err =
            try_coin_reservation_tx(&mut test_cluster, coin_reservation, sender1, sender1, gas1)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("Transaction Expired"));
    }

    // Verify transaction is rejected if the reservation amount is zero.
    {
        let coin_reservation = encode_coin_reservation(0, 0);

        let err =
            try_coin_reservation_tx(&mut test_cluster, coin_reservation, sender1, sender1, gas1)
                .await
                .unwrap_err();
        assert!(
            err.to_string()
                .contains("reservation amount must be non-zero")
        );
    }

    // Verify the transaction is rejected if the accumulator object is not owned by the sender.
    {
        let coin_reservation = encode_coin_reservation(0, 100);

        let recipient = SuiAddress::random_for_testing_only();
        let err = try_coin_reservation_tx(
            &mut test_cluster,
            coin_reservation,
            sender2,
            recipient,
            gas2,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains(format!("is owned by {}, not sender {}", sender1, sender2).as_str())
        );
    }

    // Verify that invalid epoch for coin reservation in gas is rejected.
    {
        let coin_reservation = encode_coin_reservation(3, 10000000);

        let err =
            try_coin_reservation_tx(&mut test_cluster, gas1, sender1, sender1, coin_reservation)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("Transaction Expired"));
    }

    // Verify that zero amount for coin reservation in gas is rejected.
    {
        let coin_reservation = encode_coin_reservation(0, 0);

        let err =
            try_coin_reservation_tx(&mut test_cluster, gas1, sender1, sender1, coin_reservation)
                .await
                .unwrap_err();
        assert!(
            err.to_string()
                .contains("reservation amount must be non-zero")
        );
    }

    // Verify gas budget is enforced with coin reservations.
    {
        let coin_reservation = encode_coin_reservation(0, 100);

        let err = try_coin_reservation_tx(
            &mut test_cluster,
            coin_reservation,
            sender1,
            sender1,
            coin_reservation,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Balance of gas object 100 is lower than the needed amount")
        );
    }

    // Verify that total reservation limit is enforced for coin reservations, including gas reservations.
    {
        // 1 gas reservation
        let gas_reservation = encode_coin_reservation(0, 10000000);

        // plus 1 regular reservation
        let mut tx_builder = TestTransactionBuilder::new(sender1, gas_reservation, rgp)
            .transfer_sui_to_address_balance(
                FundSource::address_fund_with_reservation(1),
                vec![(1, sender1)],
            );

        // plus 9 coin reservations
        for _ in 0..9 {
            let random_object_id = ObjectID::random();

            let coin = ParsedObjectRefWithdrawal::new(random_object_id, 0, 100)
                .encode(SequenceNumber::new(), chain_id);

            tx_builder = tx_builder.transfer(FullObjectRef::from_fastpath_ref(coin), sender1);
        }

        let tx = tx_builder.build();

        let err = test_cluster
            .sign_and_execute_transaction_directly(&tx)
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("Maximum number of balance withdraw reservations is 10")
        );
    }
}

#[sim_test]
async fn test_coin_reservation_gating() {
    let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut cfg| {
        cfg.create_root_accumulator_object_for_testing();
        cfg.enable_accumulators_for_testing();
        cfg
    });

    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    let chain_id = test_cluster.get_chain_identifier();
    let context = &mut test_cluster.wallet;

    let (sender, gas) = get_sender_and_one_gas(context).await;

    // compute the sender's SUI accumulator object id
    let accumulator_obj_id = AccumulatorValue::get_field_id(
        sender,
        &Balance::type_tag(sui_types::gas_coin::GAS::type_tag()),
    )
    .unwrap();

    let encode_coin_reservation = |epoch: u64, amount: u64| {
        ParsedObjectRefWithdrawal::new(*accumulator_obj_id.inner(), epoch, amount)
            .encode(SequenceNumber::new(), chain_id)
    };

    // Verify transaction is rejected if coin reservation is not enabled.
    {
        let coin_reservation = encode_coin_reservation(0, 1);

        let err = try_coin_reservation_tx(&mut test_cluster, coin_reservation, sender, sender, gas)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("coin reservation backward compatibility layer is not enabled")
        );
    }
}

#[sim_test]
async fn test_valid_coin_reservation_transfers() {
    let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut cfg| {
        cfg.create_root_accumulator_object_for_testing();
        cfg.enable_accumulators_for_testing();
        cfg.enable_coin_reservation_for_testing();
        cfg
    });

    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    let rgp = test_cluster.get_reference_gas_price().await;
    let chain_id = test_cluster.get_chain_identifier();
    let context = &mut test_cluster.wallet;

    let (sender, gas) = get_sender_and_one_gas(context).await;

    // send 1000 gas from the gas coins to the balances
    let tx = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_sui_to_address_balance(FundSource::coin(gas), vec![(1000, sender)])
        .build();

    let (_, effects) = test_cluster
        .sign_and_execute_transaction_directly(&tx)
        .await
        .unwrap();
    let gas = effects.gas_object().0;

    // compute the sender's SUI accumulator object id
    let accumulator_obj_id = AccumulatorValue::get_field_id(
        sender,
        &Balance::type_tag(sui_types::gas_coin::GAS::type_tag()),
    )
    .unwrap();

    let encode_coin_reservation = |epoch: u64, amount: u64| {
        ParsedObjectRefWithdrawal::new(*accumulator_obj_id.inner(), epoch, amount)
            .encode(SequenceNumber::new(), chain_id)
    };
    let coin_reservation = encode_coin_reservation(0, 100);

    let recipient = SuiAddress::random_for_testing_only();

    let gas = {
        let tx = TestTransactionBuilder::new(sender, gas, rgp)
            .transfer(
                FullObjectRef::from_fastpath_ref(coin_reservation),
                recipient,
            )
            .build();
        let signed_tx = test_cluster.wallet.sign_transaction(&tx).await;

        let res = test_cluster
            .wallet
            .execute_transaction_may_fail(signed_tx)
            .await
            .unwrap();

        assert!(res.effects.as_ref().unwrap().status().is_ok());
        res.effects.unwrap().gas_object().reference.to_object_ref()
    };

    // do the same but split the coin first
    let _gas = {
        let res =
            try_coin_reservation_tx(&mut test_cluster, coin_reservation, sender, recipient, gas)
                .await
                .unwrap();
        assert!(res.effects.as_ref().unwrap().status().is_ok());
        res.effects.unwrap().gas_object().reference.to_object_ref()
    };

    // ensure both balances arrived at the recipient
    let recipient_balance = test_cluster
        .fullnode_handle
        .rpc_client
        .get_balance(recipient, Some("0x2::sui::SUI".to_string()))
        .await
        .unwrap();
    // 100 from coin transfer, 1 from coin reservation
    assert_eq!(recipient_balance.total_balance, 100 + 1);
}

#[sim_test]
async fn test_valid_coin_reservation_gas_payments() {
    let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut cfg| {
        cfg.create_root_accumulator_object_for_testing();
        cfg.enable_accumulators_for_testing();
        cfg.enable_coin_reservation_for_testing();
        cfg
    });

    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    let rgp = test_cluster.get_reference_gas_price().await;
    let chain_id = test_cluster.get_chain_identifier();
    let context = &mut test_cluster.wallet;

    let (sender, gas) = get_sender_and_one_gas(context).await;

    let budget = 5000000000;
    // send 1000 gas from the gas coins to the balances
    let tx = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_sui_to_address_balance(FundSource::coin(gas), vec![(budget + 100, sender)])
        .build();

    let (_, effects) = test_cluster
        .sign_and_execute_transaction_directly(&tx)
        .await
        .unwrap();
    let gas = effects.gas_object().0;

    // compute the sender's SUI accumulator object id
    let accumulator_obj_id = AccumulatorValue::get_field_id(
        sender,
        &Balance::type_tag(sui_types::gas_coin::GAS::type_tag()),
    )
    .unwrap();

    let encode_coin_reservation = |epoch: u64, amount: u64| {
        ParsedObjectRefWithdrawal::new(*accumulator_obj_id.inner(), epoch, amount)
            .encode(SequenceNumber::new(), chain_id)
    };
    let coin_reservation = encode_coin_reservation(0, 1);
    let ab_gas = encode_coin_reservation(0, budget);

    let recipient = SuiAddress::random_for_testing_only();

    let res = try_coin_reservation_tx(
        &mut test_cluster,
        coin_reservation,
        sender,
        recipient,
        ab_gas,
    )
    .await
    .unwrap();
    assert!(res.effects.as_ref().unwrap().status().is_ok());
    let gas_charge = res.effects.as_ref().unwrap().gas_cost_summary().gas_used();
    dbg!(res.effects.unwrap().gas_object().reference.to_object_ref());

    // ensure both balances arrived at the recipient
    let recipient_balance = test_cluster.get_sui_balance(recipient).await;

    // 1 MIST transferred.
    assert_eq!(recipient_balance.total_balance, 1);

    let sender_balance = test_cluster
        .fullnode_handle
        .rpc_client
        .get_balance(sender, Some("0x2::sui::SUI".to_string()))
        .await
        .unwrap();
    // 1 MIST transferred.
    assert_eq!(sender_balance.total_balance, 1);
}

async fn try_coin_reservation_tx(
    test_cluster: &mut TestCluster,
    coin_reservation: ObjectRef,
    sender: SuiAddress,
    recipient: SuiAddress,
    gas: ObjectRef,
) -> anyhow::Result<SuiTransactionBlockResponse> {
    let rgp = test_cluster.get_reference_gas_price().await;

    let tx = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_sui_to_address_balance(FundSource::coin(coin_reservation), vec![(1, recipient)])
        .build();

    let signed_tx = test_cluster.wallet.sign_transaction(&tx).await;
    test_cluster
        .wallet
        .execute_transaction_may_fail(signed_tx)
        .await
}
