//! Ledger-level tests for `supplier_accounts` (migrations 0025–0029 and
//! `src/supplier_accounts/repository.rs`).

use organox::payments::model::WalletMovementKind;
use organox::payments::repository::{self as wallets, NewMethod, NewWalletMovement};
use organox::supplier_accounts::model::SupplierAccountEntryKind;
use organox::supplier_accounts::repository::{
    self as ledger, NewSupplierCreditRefund, NewSupplierPayment, RepoError,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use uuid::Uuid;

fn setup_actor() -> Uuid {
    Uuid::now_v7()
}

async fn setup_supplier(pool: &PgPool, name: &str, credit_limit: Option<Decimal>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO suppliers (id, name, credit_limit) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(name)
        .bind(credit_limit)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn setup_completed_purchase(
    pool: &PgPool,
    supplier_id: Uuid,
    supplier_name: &str,
    total: Decimal,
    actor: Uuid,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, supplier_name, total, created_by, completed_by, completed_at)
         VALUES ($1, 'completed', $2, $3, $4, $5, $5, now())",
    )
    .bind(id)
    .bind(supplier_id)
    .bind(supplier_name)
    .bind(total)
    .bind(actor)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn setup_method(pool: &PgPool, name: &str) -> (Uuid, Uuid) {
    let wallet = wallets::create_wallet(pool, &format!("{name} wallet"))
        .await
        .unwrap();
    let method = wallets::create_method(
        pool,
        NewMethod {
            name,
            wallet_id: wallet,
        },
    )
    .await
    .unwrap();
    (wallet, method)
}

async fn insert_supplier_movement(
    pool: &PgPool,
    wallet_id: Uuid,
    method_id: Uuid,
    supplier_id: Uuid,
    kind: WalletMovementKind,
    amount: Decimal,
    actor: Uuid,
) -> Uuid {
    wallets::insert_movement(
        pool,
        NewWalletMovement {
            wallet_id,
            kind,
            amount,
            sale_id: None,
            purchase_id: None,
            payment_method_id: Some(method_id),
            credit_note_id: None,
            purchase_return_id: None,
            customer_id: None,
            supplier_id: Some(supplier_id),
            note: Some("ledger test"),
            created_by: Some(actor),
        },
    )
    .await
    .unwrap()
}

#[sqlx::test]
async fn charge_payment_allocation_and_invariant_hold(pool: PgPool) {
    let actor = setup_actor();
    let supplier = setup_supplier(&pool, "Acme", None).await;
    let p1 = setup_completed_purchase(&pool, supplier, "Acme", dec!(200.00), actor).await;
    let p2 = setup_completed_purchase(&pool, supplier, "Acme", dec!(100.00), actor).await;
    let (wallet, method) = setup_method(&pool, "Bank").await;

    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, p1, dec!(200.00), actor)
        .await
        .unwrap();
    ledger::record_charge(&mut tx, supplier, p2, dec!(100.00), actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Pay 250: allocate 200 to p1, leave 50 as unapplied credit.
    let mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(250.00),
        actor,
    )
    .await;

    let mut tx = pool.begin().await.unwrap();
    let rec = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: mv,
            amount: dec!(250.00),
            allocations: &[(p1, dec!(200.00))],
            idempotency_key: Some("pay-1"),
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    assert!(!rec.replayed);

    // Replay returns the same entry without rewriting.
    let replay = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: mv,
            amount: dec!(250.00),
            allocations: &[(p1, dec!(200.00))],
            idempotency_key: Some("pay-1"),
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.entry_id, rec.entry_id);
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(50.00));
    assert_eq!(pos.unapplied_credit, dec!(50.00));
    assert_eq!(pos.outstanding_total, dec!(100.00));
    assert!(pos.is_consistent());

    // Apply available credit (50) automatically to p2.
    let mut tx = pool.begin().await.unwrap();
    ledger::lock_supplier(&mut tx, supplier).await.unwrap();
    let applied = ledger::apply_available_credit(&mut tx, supplier, p2, dec!(100.00))
        .await
        .unwrap();
    assert_eq!(applied, dec!(50.00));
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(50.00));
    assert_eq!(pos.unapplied_credit, dec!(0.00));
    assert_eq!(pos.outstanding_total, dec!(50.00));
    assert!(pos.is_consistent());
}

#[sqlx::test]
async fn credit_limit_authorization_enforces_null_zero_and_cap(pool: PgPool) {
    let actor = setup_actor();
    let cash_only = setup_supplier(&pool, "CashOnly", Some(dec!(0.00))).await;
    let capped = setup_supplier(&pool, "Capped", Some(dec!(100.00))).await;
    let unlimited = setup_supplier(&pool, "Unlimited", None).await;

    // Cash-only rejects any positive charge, allows zero.
    let mut tx = pool.begin().await.unwrap();
    assert!(
        ledger::authorize_credit(&mut tx, cash_only, dec!(0.00))
            .await
            .is_ok()
    );
    let err = ledger::authorize_credit(&mut tx, cash_only, dec!(1.00))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RepoError::CreditLimitExceeded {
            requested,
            available
        } if requested == dec!(1.00) && available == dec!(0.00)
    ));
    tx.rollback().await.unwrap();

    // Capped allows up to 100, rejects 100.01.
    let mut tx = pool.begin().await.unwrap();
    assert!(
        ledger::authorize_credit(&mut tx, capped, dec!(100.00))
            .await
            .is_ok()
    );
    let err = ledger::authorize_credit(&mut tx, capped, dec!(100.01))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RepoError::CreditLimitExceeded {
            requested,
            available
        } if requested == dec!(100.01) && available == dec!(100.00)
    ));
    tx.rollback().await.unwrap();

    // Unlimited allows large amount.
    let mut tx = pool.begin().await.unwrap();
    assert!(
        ledger::authorize_credit(&mut tx, unlimited, dec!(999999.00))
            .await
            .is_ok()
    );
    tx.rollback().await.unwrap();

    // Inactive supplier is rejected even when unlimited.
    sqlx::query("UPDATE suppliers SET is_active = false WHERE id = $1")
        .bind(unlimited)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::authorize_credit(&mut tx, unlimited, dec!(10.00))
        .await
        .unwrap_err();
    assert!(matches!(err, RepoError::SupplierInactive));
    let _ = actor;
}

#[sqlx::test]
async fn credit_refund_and_adjustments_enforce_guards(pool: PgPool) {
    let actor = setup_actor();
    let supplier = setup_supplier(&pool, "RefundCo", None).await;
    let (wallet, method) = setup_method(&pool, "Cash").await;

    // Prepay 100 without allocations -> balance -100, unapplied_credit 100.
    let pay_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(100.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Adjustment while holding credit is rejected.
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_adjustment(
        &mut tx,
        supplier,
        SupplierAccountEntryKind::AdjustmentIn,
        dec!(10.00),
        "bad while negative",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RepoError::AdjustmentCrossesZero { .. }));
    tx.rollback().await.unwrap();

    // Supplier refunds 60 back into wallet.
    let ref_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(60.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: supplier,
            wallet_movement_id: ref_mv,
            amount: dec!(60.00),
            idempotency_key: Some("ref-1"),
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(-40.00));
    assert_eq!(pos.unapplied_credit, dec!(40.00));
    assert!(pos.is_consistent());

    // Wallet net balance: -100 + 60 = -40.
    let wb = wallets::get_wallet(&pool, wallet).await.unwrap().balance;
    assert_eq!(wb, dec!(-40.00));

    // Trying to refund 50 (> 40 remaining) fails.
    let over_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(50.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: supplier,
            wallet_movement_id: over_mv,
            amount: dec!(50.00),
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        RepoError::CreditRefundExceedsCredit {
            requested,
            available
        } if requested == dec!(50.00) && available == dec!(40.00)
    ));
}

#[sqlx::test]
async fn append_only_and_wallet_exclusivity_triggers_hold(pool: PgPool) {
    let actor = setup_actor();
    let supplier = setup_supplier(&pool, "AppendCo", None).await;
    let p = setup_completed_purchase(&pool, supplier, "AppendCo", dec!(50.00), actor).await;

    let mut tx = pool.begin().await.unwrap();
    let entry_id = ledger::record_charge(&mut tx, supplier, p, dec!(50.00), actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Append-only refuses UPDATE and DELETE.
    let upd = sqlx::query("UPDATE supplier_account_entries SET amount = 10.00 WHERE id = $1")
        .bind(entry_id)
        .execute(&pool)
        .await;
    assert!(upd.is_err());

    let del = sqlx::query("DELETE FROM supplier_account_entries WHERE id = $1")
        .bind(entry_id)
        .execute(&pool)
        .await;
    assert!(del.is_err());

    // Cross-column exclusivity rejects setting both purchase_id and supplier_id.
    let (wallet, method) = setup_method(&pool, "Cash2").await;
    let bad = sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, kind, amount, purchase_id, supplier_id, payment_method_id)
         VALUES ($1, $2, 'supplier_account_payment', 10.00, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(wallet)
    .bind(p)
    .bind(supplier)
    .bind(method)
    .execute(&pool)
    .await;
    assert!(bad.is_err());
}

#[sqlx::test]
async fn write_guards_reject_invalid_backing_charges_payments_allocations_and_adjustments(
    pool: PgPool,
) {
    let actor = setup_actor();
    let supplier = setup_supplier(&pool, "GuardCo", None).await;
    let other_supplier = setup_supplier(&pool, "OtherCo", None).await;
    let (wallet, method) = setup_method(&pool, "Bank").await;

    // --- record_charge guards: upfront-paid bound and duplicate charge ---
    let p_partial = setup_completed_purchase(&pool, supplier, "GuardCo", dec!(100.00), actor).await;
    wallets::insert_movement(
        &pool,
        NewWalletMovement {
            wallet_id: wallet,
            kind: WalletMovementKind::PurchasePayment,
            amount: dec!(40.00),
            sale_id: None,
            purchase_id: Some(p_partial),
            payment_method_id: Some(method),
            credit_note_id: None,
            purchase_return_id: None,
            customer_id: None,
            supplier_id: None,
            note: Some("upfront cash"),
            created_by: Some(actor),
        },
    )
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_charge(&mut tx, supplier, p_partial, dec!(60.01), actor)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RepoError::ChargeExceedsPurchaseTotal {
            requested,
            available
        } if requested == dec!(60.01) && available == dec!(60.00)
    ));
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let charge_entry_id = ledger::record_charge(&mut tx, supplier, p_partial, dec!(60.00), actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_charge(&mut tx, supplier, p_partial, dec!(10.00), actor)
        .await
        .unwrap_err();
    assert!(matches!(err, RepoError::AlreadyCharged));
    tx.rollback().await.unwrap();

    // --- ensure_movement_backing: missing, amount mismatch, wrong kind, wrong supplier ---
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: Uuid::now_v7(),
            amount: dec!(10.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("missing movement must fail");
    assert!(matches!(err, RepoError::MovementNotFound));
    tx.rollback().await.unwrap();

    let pay_mv_100 = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(100.00),
        actor,
    )
    .await;

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv_100,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("amount mismatch must fail");
    assert!(matches!(
        err,
        RepoError::EntryDoesNotMatchMovement { movement, entry }
            if movement == dec!(100.00) && entry == dec!(50.00)
    ));
    tx.rollback().await.unwrap();

    let refund_mv_10 = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(10.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: refund_mv_10,
            amount: dec!(10.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("wrong movement kind must fail");
    assert!(matches!(
        err,
        RepoError::MovementNotFromSupplierAccount { ref kind } if kind == "supplier_account_refund"
    ));
    tx.rollback().await.unwrap();

    let other_mv_10 = insert_supplier_movement(
        &pool,
        wallet,
        method,
        other_supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(10.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: other_mv_10,
            amount: dec!(10.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("movement of another supplier must fail");
    assert!(matches!(err, RepoError::MovementOfAnotherSupplier));
    tx.rollback().await.unwrap();

    // --- record_payment guards: duplicate allocation, per-purchase outstanding, over-allocation, AlreadyRecorded ---
    let p_second = setup_completed_purchase(&pool, supplier, "GuardCo", dec!(80.00), actor).await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, p_second, dec!(80.00), actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv_100,
            amount: dec!(100.00),
            allocations: &[(p_partial, dec!(10.00)), (p_partial, dec!(10.00))],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("duplicate allocation must fail");
    assert!(matches!(
        err,
        RepoError::DuplicateAllocation { purchase_id } if purchase_id == p_partial
    ));
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv_100,
            amount: dec!(100.00),
            allocations: &[(p_partial, dec!(60.01))],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("exceeding purchase outstanding must fail");
    assert!(matches!(
        err,
        RepoError::OutstandingExceeded {
            requested,
            outstanding
        } if requested == dec!(60.01) && outstanding == dec!(60.00)
    ));
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv_100,
            amount: dec!(100.00),
            allocations: &[(p_partial, dec!(60.00)), (p_second, dec!(50.00))],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("over-allocation must fail");
    assert!(matches!(
        err,
        RepoError::PaymentOverAllocated { allocated, paid }
            if allocated == dec!(110.00) && paid == dec!(100.00)
    ));
    tx.rollback().await.unwrap();

    // Commit pay_mv_100 (allocated to p_partial), then reuse the same wallet_movement_id -> AlreadyRecorded.
    let mut tx = pool.begin().await.unwrap();
    let pay_entry_id = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv_100,
            amount: dec!(100.00),
            allocations: &[(p_partial, dec!(60.00))],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv_100,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("reusing wallet_movement_id must fail");
    assert!(matches!(err, RepoError::AlreadyRecorded));
    tx.rollback().await.unwrap();

    // --- allocate guards: NotAPayment, AllocationExceedsPayment, AllocationExceedsCredit ---
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::allocate(&mut tx, charge_entry_id, p_second, dec!(10.00))
        .await
        .unwrap_err();
    assert!(matches!(err, RepoError::NotAPayment));
    tx.rollback().await.unwrap();

    // pay_entry_id has 60 allocated out of 100 -> allocating 40.01 exceeds the payment itself.
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::allocate(&mut tx, pay_entry_id, p_second, dec!(40.01))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RepoError::AllocationExceedsPayment { paid, allocated }
            if paid == dec!(100.00) && allocated == dec!(60.00)
    ));
    tx.rollback().await.unwrap();

    // Trigger AllocationExceedsCredit: a fresh supplier prepays 100, receives a 60 credit refund
    // (leaving unapplied_credit = 40 while the payment entry still has 100 unallocated), then
    // tries to allocate 50 to an open purchase.
    let credit_supplier = setup_supplier(&pool, "CreditBoundCo", None).await;
    let prepay_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        credit_supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(100.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let prepay_entry_id = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: credit_supplier,
            wallet_movement_id: prepay_mv,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let ref_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        credit_supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(60.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: credit_supplier,
            wallet_movement_id: ref_mv,
            amount: dec!(60.00),
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let p_credit =
        setup_completed_purchase(&pool, credit_supplier, "CreditBoundCo", dec!(80.00), actor).await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, credit_supplier, p_credit, dec!(80.00), actor)
        .await
        .unwrap();
    let err = ledger::allocate(&mut tx, prepay_entry_id, p_credit, dec!(50.00))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RepoError::AllocationExceedsCredit {
            requested,
            available
        } if requested == dec!(50.00) && available == dec!(40.00)
    ));
    tx.rollback().await.unwrap();

    // --- record_adjustment: non-manual kind, blank note, happy path (in + out), crossing zero ---
    let adj_supplier = setup_supplier(&pool, "AdjCo", None).await;

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_adjustment(
        &mut tx,
        adj_supplier,
        SupplierAccountEntryKind::Payment,
        dec!(10.00),
        "wrong kind",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RepoError::NotAnAdjustment));
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_adjustment(
        &mut tx,
        adj_supplier,
        SupplierAccountEntryKind::AdjustmentIn,
        dec!(10.00),
        "   ",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RepoError::AdjustmentNoteRequired));
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    ledger::record_adjustment(
        &mut tx,
        adj_supplier,
        SupplierAccountEntryKind::AdjustmentIn,
        dec!(75.00),
        "opening balance",
        actor,
    )
    .await
    .unwrap();
    ledger::record_adjustment(
        &mut tx,
        adj_supplier,
        SupplierAccountEntryKind::AdjustmentOut,
        dec!(25.00),
        "invoice discount",
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, adj_supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(50.00));
    assert_eq!(pos.unapplied_credit, dec!(0.00));
    assert!(pos.is_consistent());

    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_adjustment(
        &mut tx,
        adj_supplier,
        SupplierAccountEntryKind::AdjustmentOut,
        dec!(50.01),
        "too large",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        RepoError::AdjustmentCrossesZero { balance } if balance == dec!(50.00)
    ));
    tx.rollback().await.unwrap();

    let pos_after = ledger::account_position(&pool, adj_supplier).await.unwrap();
    assert_eq!(pos_after.balance, dec!(50.00));
}

#[sqlx::test]
async fn lock_supplier_maps_lock_timeout_to_supplier_busy_and_aborts_tx(pool: PgPool) {
    let supplier = setup_supplier(&pool, "BusyLedgerCo", None).await;

    // Another transaction holds the supplier advisory lock (seed 1).
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 1))")
        .bind(supplier.to_string())
        .execute(&mut *holder)
        .await
        .unwrap();

    let mut waiter = pool.begin().await.unwrap();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        ledger::lock_supplier(&mut waiter, supplier),
    )
    .await
    .expect("lock_supplier did not time out within the bound")
    .unwrap_err();
    assert!(matches!(err, RepoError::SupplierBusy));

    // PostgreSQL marks the timed-out transaction as aborted (25P02: rollback-only).
    let follow_up = sqlx::query("SELECT 1").execute(&mut *waiter).await;
    assert!(matches!(
        follow_up,
        Err(sqlx::Error::Database(ref db)) if db.code().as_deref() == Some("25P02")
    ));

    waiter.rollback().await.unwrap();
    holder.rollback().await.unwrap();
}

#[sqlx::test]
async fn purchase_return_and_return_credit_preserve_invariants_and_apply_to_future_purchases(
    pool: PgPool,
) {
    let actor = setup_actor();
    let supplier = setup_supplier(&pool, "ReturnLedgerCo", None).await;
    let p1 = setup_completed_purchase(&pool, supplier, "ReturnLedgerCo", dec!(200.00), actor).await;
    let p2 = setup_completed_purchase(&pool, supplier, "ReturnLedgerCo", dec!(100.00), actor).await;
    let (wallet, method) = setup_method(&pool, "Bank").await;

    // Charge p1 (200) on account.
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, p1, dec!(200.00), actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Insert a purchase_return document for 120 (80 debt cancel + 40 return credit
    // in separate steps, proving both ledger entry kinds and append-only triggers).
    let ret1 = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_returns (id, purchase_id, total, surplus_mode, reason, created_by)
         VALUES ($1, $2, 120.00, 'supplier_credit', 'Damaged batch', $3)",
    )
    .bind(ret1)
    .bind(p1)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();

    // Append-only trigger blocks UPDATE and DELETE on purchase_returns.
    assert!(
        sqlx::query("UPDATE purchase_returns SET reason = 'edited' WHERE id = $1")
            .bind(ret1)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM purchase_returns WHERE id = $1")
            .bind(ret1)
            .execute(&pool)
            .await
            .is_err()
    );

    // Cancel 80 of p1's 200 debt and grant 40 as supplier return credit.
    let mut tx = pool.begin().await.unwrap();
    ledger::record_purchase_return(
        &mut tx,
        supplier,
        p1,
        ret1,
        dec!(80.00),
        Some("ret1"),
        actor,
    )
    .await
    .unwrap();
    ledger::record_purchase_return_credit(
        &mut tx,
        supplier,
        ret1,
        dec!(40.00),
        Some("ret1 credit"),
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Position: balance = 200 - 80 - 40 = 80, U = 40, outstanding(p1) = 120.
    // Identity: 80 + 40 == 120 + 0.
    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(80.00));
    assert_eq!(pos.unapplied_credit, dec!(40.00));
    assert_eq!(pos.outstanding_total, dec!(120.00));
    assert!(pos.is_consistent());

    let open = ledger::outstanding_by_purchase(&pool, supplier)
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].purchase_id, p1);
    assert_eq!(open[0].charged, dec!(200.00));
    assert_eq!(open[0].cancelled, dec!(80.00));
    assert_eq!(open[0].allocated, dec!(0.00));
    assert_eq!(open[0].outstanding, dec!(120.00));

    // Trying to allocate 120.01 to p1 fails both in Rust and at the DB ceiling.
    let pay_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(150.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: pay_mv,
            amount: dec!(150.00),
            allocations: &[(p1, dec!(120.01))],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .err()
    .expect("over-allocation past return cancellation must fail");
    assert!(matches!(
        err,
        RepoError::OutstandingExceeded { requested, outstanding }
            if requested == dec!(120.01) && outstanding == dec!(120.00)
    ));
    tx.rollback().await.unwrap();

    // Charge p2 (100) on account and apply available credit -> consumes the 40 return credit!
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, p2, dec!(100.00), actor)
        .await
        .unwrap();
    let applied = ledger::apply_available_credit(&mut tx, supplier, p2, dec!(100.00))
        .await
        .unwrap();
    assert_eq!(applied, dec!(40.00));
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(180.00));
    assert_eq!(pos.unapplied_credit, dec!(0.00));
    assert_eq!(pos.outstanding_total, dec!(180.00));
    assert!(pos.is_consistent());

    // Raw DB deferred trigger refuses a purchase_return entry exceeding charged - allocated.
    let ret2 = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_returns (id, purchase_id, total, surplus_mode, reason, created_by)
         VALUES ($1, $2, 70.00, 'supplier_credit', 'Over-cancel trigger test', $3)",
    )
    .bind(ret2)
    .bind(p2)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO supplier_account_entries
            (id, supplier_id, kind, amount, purchase_id, purchase_return_id, created_by)
         VALUES ($1, $2, 'purchase_return', -60.01, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(supplier)
    .bind(p2)
    .bind(ret2)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(tx.commit().await.is_err());
}

#[sqlx::test]
async fn supplier_adjustment_in_reconciles_after_purchase_return_and_protects_return_credit(
    pool: PgPool,
) {
    let actor = Uuid::now_v7();
    let (wallet, method) = setup_method(&pool, "Bank").await;
    let supplier = setup_supplier(&pool, "ReturnParityCo", None).await;
    let purchase =
        setup_completed_purchase(&pool, supplier, "ReturnParityCo", dec!(100.00), actor).await;

    // 1. Charge $100, adjustment_out $30 -> B = 70, O = 100.
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, purchase, dec!(100.00), actor)
        .await
        .unwrap();
    ledger::record_adjustment(
        &mut tx,
        supplier,
        SupplierAccountEntryKind::AdjustmentOut,
        dec!(30.00),
        "over-adjustment",
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // 2. Full purchase_return ($100) + purchase_return_credit ($40) -> B = -70, U = 40, O = 0.
    let ret = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_returns (id, purchase_id, total, surplus_mode, reason, created_by)
         VALUES ($1, $2, 140.00, 'supplier_credit', 'Return parity probe', $3)",
    )
    .bind(ret)
    .bind(purchase)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    ledger::record_purchase_return(
        &mut tx,
        supplier,
        purchase,
        ret,
        dec!(100.00),
        Some("ret cancel"),
        actor,
    )
    .await
    .unwrap();
    ledger::record_purchase_return_credit(
        &mut tx,
        supplier,
        ret,
        dec!(40.00),
        Some("ret credit"),
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // 30.01 exceeds non-credit drift (30.00) -> rejected.
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_adjustment(
        &mut tx,
        supplier,
        SupplierAccountEntryKind::AdjustmentIn,
        dec!(30.01),
        "would bury purchase_return_credit",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        RepoError::AdjustmentCrossesZero { balance } if balance == dec!(-70.00)
    ));
    tx.rollback().await.unwrap();

    // Exact 30.00 drift succeeds -> B = -40, U = 40, consistent.
    let mut tx = pool.begin().await.unwrap();
    ledger::record_adjustment(
        &mut tx,
        supplier,
        SupplierAccountEntryKind::AdjustmentIn,
        dec!(30.00),
        "reconcile drift",
        actor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, dec!(-40.00));
    assert_eq!(pos.unapplied_credit, dec!(40.00));
    assert_eq!(pos.outstanding_total, Decimal::ZERO);
    assert!(pos.is_consistent());

    // Any further adjustment_in is refused (drift = 0).
    let mut tx = pool.begin().await.unwrap();
    let err = ledger::record_adjustment(
        &mut tx,
        supplier,
        SupplierAccountEntryKind::AdjustmentIn,
        dec!(0.01),
        "drift is now zero",
        actor,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        RepoError::AdjustmentCrossesZero { balance } if balance == dec!(-40.00)
    ));
    tx.rollback().await.unwrap();

    // Refund the 40.00 return credit -> B = 0, U = 0.
    let refund_mv = insert_supplier_movement(
        &pool,
        wallet,
        method,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(40.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_credit_refund(
        &mut tx,
        ledger::NewSupplierCreditRefund {
            supplier_id: supplier,
            wallet_movement_id: refund_mv,
            amount: dec!(40.00),
            idempotency_key: None,
            note: Some("refund return credit"),
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let pos = ledger::account_position(&pool, supplier).await.unwrap();
    assert_eq!(pos.balance, Decimal::ZERO);
    assert_eq!(pos.unapplied_credit, Decimal::ZERO);
    assert!(pos.is_consistent());
}

#[sqlx::test]
async fn credit_refund_attribution_blocks_manual_allocate_handles_return_credits_and_enforces_triggers(
    pool: PgPool,
) {
    let actor = setup_actor();
    let (wallet_a, method_a) = setup_method(&pool, "Cash A").await;
    let (wallet_b, method_b) = setup_method(&pool, "Bank B").await;
    let supplier = setup_supplier(&pool, "RefundAttrCo", None).await;

    // Case 1: Payment 1 ($100, Wallet A) refunded in full, then Payment 2 ($50, Wallet B).
    let mv_p1 = insert_supplier_movement(
        &pool,
        wallet_a,
        method_a,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(100.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let p1 = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: mv_p1,
            amount: dec!(100.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let mv_ref1 = insert_supplier_movement(
        &pool,
        wallet_a,
        method_a,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(100.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let ref1 = ledger::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: supplier,
            wallet_movement_id: mv_ref1,
            amount: dec!(100.00),
            idempotency_key: None,
            note: Some("full refund"),
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mv_p2 = insert_supplier_movement(
        &pool,
        wallet_b,
        method_b,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(50.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let p2 = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: mv_p2,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let purchase1 =
        setup_completed_purchase(&pool, supplier, "RefundAttrCo", dec!(50.00), actor).await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, purchase1, dec!(50.00), actor)
        .await
        .unwrap();

    // Direct allocate on refunded Payment 1 fails with available = 0.
    let err = ledger::allocate(&mut tx, p1, purchase1, dec!(50.00))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RepoError::AllocationExceedsCredit {
            requested,
            available
        } if requested == dec!(50.00) && available == Decimal::ZERO
    ));

    // Direct SQL insert on refunded Payment 1 fails at COMMIT via trigger.
    sqlx::query(
        "INSERT INTO supplier_account_allocations (id, entry_id, purchase_id, amount) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(p1)
    .bind(purchase1)
    .bind(dec!(50.00))
    .execute(&mut *tx)
    .await
    .unwrap();
    let commit_err = tx.commit().await.unwrap_err();
    assert!(
        commit_err
            .to_string()
            .contains("allocations (50.00) plus refunds (100.00) exceed"),
        "expected payment ceiling trigger error, got: {commit_err}"
    );

    // Append-only trigger on supplier_account_credit_refund_allocations.
    let del_err = sqlx::query(
        "DELETE FROM supplier_account_credit_refund_allocations WHERE refund_entry_id = $1",
    )
    .bind(ref1)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        del_err.to_string().contains("append-only"),
        "expected append-only error, got: {del_err}"
    );

    // Unattributed credit_refund raw SQL insert fails at COMMIT via completeness trigger.
    let mv_orphan = insert_supplier_movement(
        &pool,
        wallet_b,
        method_b,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(10.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'credit_refund', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(supplier)
    .bind(dec!(10.00))
    .bind(mv_orphan)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .unwrap();
    let orphan_err = tx.commit().await.unwrap_err();
    assert!(
        orphan_err.to_string().contains("must be fully attributed"),
        "expected completeness trigger error, got: {orphan_err}"
    );

    // apply_available_credit on purchase1 consumes Payment 2 ($50).
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, purchase1, dec!(50.00), actor)
        .await
        .unwrap();
    let applied = ledger::apply_available_credit(&mut tx, supplier, purchase1, dec!(50.00))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(applied, dec!(50.00));

    let allocs1: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry_id, amount FROM supplier_account_allocations WHERE purchase_id = $1",
    )
    .bind(purchase1)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(allocs1, vec![(p2, dec!(50.00))]);

    // Case 2: Both a purchase_return_credit ($60, no wallet) and a payment ($50, Wallet B) exist.
    // Refunding $50 into Wallet B consumes the Wallet B payment first (same wallet), then
    // refunding $20 into Wallet A consumes $20 of the purchase_return_credit, leaving $40.
    let ret = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_returns (id, purchase_id, total, surplus_mode, reason, created_by)
         VALUES ($1, $2, 60.00, 'supplier_credit', 'Supplier credit probe', $3)",
    )
    .bind(ret)
    .bind(purchase1)
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    let mv_p3 = insert_supplier_movement(
        &pool,
        wallet_b,
        method_b,
        supplier,
        WalletMovementKind::SupplierAccountPayment,
        dec!(50.00),
        actor,
    )
    .await;

    let mut tx = pool.begin().await.unwrap();
    let rc_id = ledger::record_purchase_return_credit(
        &mut tx,
        supplier,
        ret,
        dec!(60.00),
        Some("return credit"),
        actor,
    )
    .await
    .unwrap();
    let p3 = ledger::record_payment(
        &mut tx,
        NewSupplierPayment {
            supplier_id: supplier,
            wallet_movement_id: mv_p3,
            amount: dec!(50.00),
            allocations: &[],
            idempotency_key: None,
            note: None,
            created_by: actor,
        },
    )
    .await
    .unwrap()
    .entry_id;
    tx.commit().await.unwrap();

    let mv_ref_b = insert_supplier_movement(
        &pool,
        wallet_b,
        method_b,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(50.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    let ref_b = ledger::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: supplier,
            wallet_movement_id: mv_ref_b,
            amount: dec!(50.00),
            idempotency_key: None,
            note: Some("refund bank"),
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let consumed_b: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT source_entry_id, amount FROM supplier_account_credit_refund_allocations WHERE refund_entry_id = $1",
    )
    .bind(ref_b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(consumed_b, vec![(p3, dec!(50.00))]);

    let mv_ref_a = insert_supplier_movement(
        &pool,
        wallet_a,
        method_a,
        supplier,
        WalletMovementKind::SupplierAccountRefund,
        dec!(20.00),
        actor,
    )
    .await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_credit_refund(
        &mut tx,
        NewSupplierCreditRefund {
            supplier_id: supplier,
            wallet_movement_id: mv_ref_a,
            amount: dec!(20.00),
            idempotency_key: None,
            note: Some("partial refund of return credit"),
            created_by: actor,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let purchase2 =
        setup_completed_purchase(&pool, supplier, "RefundAttrCo", dec!(60.00), actor).await;
    let mut tx = pool.begin().await.unwrap();
    ledger::record_charge(&mut tx, supplier, purchase2, dec!(60.00), actor)
        .await
        .unwrap();
    let applied2 = ledger::apply_available_credit(&mut tx, supplier, purchase2, dec!(60.00))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(applied2, dec!(40.00));

    let allocs2: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry_id, amount FROM supplier_account_allocations WHERE purchase_id = $1",
    )
    .bind(purchase2)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(allocs2, vec![(rc_id, dec!(40.00))]);
}
