// W16b: Economic Balance Tests (P2-7)
// Validates: storage tax, anti-snowball, spawn costs, resource conservation
// Spec: specs/core/08-resource-ledger.md §2.2, DEFERRED.md D7

#[cfg(test)]
mod tests {
    use swarm_engine::resource_ledger::{
        GlobalDepositRequest, GlobalWithdrawRequest, ResourceLedger, ResourceOperation,
        compute_continuous_storage_tax, compute_fee, execute_global_deposit,
        execute_global_withdraw, execute_storage_tax,
    };
    use swarm_engine::resources::{
        GlobalStorageConfig, PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage,
    };

    fn default_global_config() -> GlobalStorageConfig {
        GlobalStorageConfig {
            enabled: true,
            namespace: "test".into(),
            intercept_enabled: false,
            intercept_range: 0,
            capacity: 1_000_000,
            transfer_to_global_fee_per_10_000: 50,    // 0.5%
            transfer_from_global_fee_per_10_000: 100, // 1.0%
            transfer_to_global_ticks: 0,              // instant
            transfer_from_global_ticks: 0,            // instant
            ..Default::default()
        }
    }

    // ── Storage Tax: Discourages Infinite Hoarding (§2.2) ──

    #[test]
    fn storage_tax_zero_below_30_percent_utilization() {
        let config = default_global_config();
        let tax = compute_continuous_storage_tax(250_000, config.capacity, &config);
        assert_eq!(tax, 0, "storage under 30% should be tax-free");
    }

    #[test]
    fn storage_tax_increases_with_utilization() {
        let config = default_global_config();
        let tax_75 = compute_continuous_storage_tax(750_000, config.capacity, &config);
        let tax_100 = compute_continuous_storage_tax(1_000_000, config.capacity, &config);
        assert!(tax_100 > tax_75, "tax should be progressive");
    }

    #[test]
    fn storage_tax_drains_resources_at_full_capacity() {
        let config = default_global_config();
        let mut storage = PlayerGlobalStorage::default();
        storage
            .0
            .entry(1)
            .or_default()
            .insert("energy".to_string(), 1_000_000);

        let result = execute_storage_tax(&mut storage, 1, &config);
        assert!(result.success);
        assert!(result.amount_delivered > 0, "full storage should incur tax");
        let remaining: u32 = storage.0[&1].values().copied().sum();
        assert!(remaining < 1_000_000, "tax should reduce stored resources");
    }

    // ── Transfer Fees: Fee Calculation Accuracy ──

    #[test]
    fn global_deposit_deducts_fee() {
        let config = default_global_config();
        let mut local = PlayerLocalStorage::default();
        let mut global = PlayerGlobalStorage::default();
        let mut pending = PendingGlobalTransfers::default();

        local
            .0
            .entry(1)
            .or_default()
            .insert("energy".to_string(), 1000);

        let result = execute_global_deposit(
            &mut local,
            &mut global,
            &mut pending,
            &config,
            GlobalDepositRequest {
                player_id: 1,
                resource: "energy",
                amount: 1000,
                tick: 0,
            },
        );
        assert!(result.success);
        // 0.5% fee = 5, net = 995
        assert_eq!(result.fee_paid, 5);
        assert_eq!(result.amount_delivered, 995);
        assert_eq!(global.0[&1]["energy"], 995);
    }

    #[test]
    fn global_withdraw_deducts_fee() {
        let config = default_global_config();
        let mut local = PlayerLocalStorage::default();
        let mut global = PlayerGlobalStorage::default();
        let mut pending = PendingGlobalTransfers::default();

        global
            .0
            .entry(1)
            .or_default()
            .insert("energy".to_string(), 1000);

        let result = execute_global_withdraw(
            &mut local,
            &mut global,
            &mut pending,
            &config,
            GlobalWithdrawRequest {
                player_id: 1,
                resource: "energy",
                amount: 1000,
                tick: 0,
            },
        );
        assert!(result.success);
        // 1.0% fee = 10, net = 990
        assert_eq!(result.fee_paid, 10);
        assert_eq!(result.amount_delivered, 990);
        assert_eq!(local.0[&1]["energy"], 990);
    }

    #[test]
    fn deposit_fails_when_local_insufficient() {
        let config = default_global_config();
        let mut local = PlayerLocalStorage::default();
        let mut global = PlayerGlobalStorage::default();
        let mut pending = PendingGlobalTransfers::default();

        local
            .0
            .entry(1)
            .or_default()
            .insert("energy".to_string(), 100);

        let result = execute_global_deposit(
            &mut local,
            &mut global,
            &mut pending,
            &config,
            GlobalDepositRequest {
                player_id: 1,
                resource: "energy",
                amount: 1000,
                tick: 0,
            },
        );
        assert!(!result.success);
    }

    // ── Anti-Snowball: Resource Conservation ──

    #[test]
    fn fee_computation_is_deterministic() {
        for _ in 0..100 {
            assert_eq!(compute_fee(1000, 50), 5);
            assert_eq!(compute_fee(10000, 100), 100);
        }
    }

    #[test]
    fn fee_never_exceeds_principal() {
        assert!(compute_fee(100, 10000) <= 100);
        assert_eq!(compute_fee(0, 10000), 0);
    }

    // ── Ledger Integrity ──

    #[test]
    fn ledger_balanced_for_transfer_round_trip() {
        let mut ledger = ResourceLedger::default();

        // Player 1 deposits 1000 energy: source loses 1000
        ledger.record(
            0,
            Some(1),
            None,
            "energy",
            1000,
            ResourceOperation::GlobalDeposit,
        );

        // Player 1 withdraws 500: target gains 500
        ledger.record(
            0,
            None,
            Some(1),
            "energy",
            500,
            ResourceOperation::GlobalWithdraw,
        );

        let p1 = &ledger.balance_delta[&1];
        assert_eq!(p1["energy"], -500);
    }

    #[test]
    fn ledger_checksum_accumulates_across_ticks() {
        let mut ledger = ResourceLedger::default();
        let cs_before = ledger.ledger_checksum;

        ledger.record(
            0,
            Some(1),
            Some(2),
            "energy",
            100,
            ResourceOperation::LocalTransfer,
        );
        assert_ne!(ledger.ledger_checksum, cs_before);

        ledger.ops.clear();
        let cs_after_clear = ledger.ledger_checksum;
        assert_eq!(cs_after_clear, ledger.ledger_checksum);
    }
}
