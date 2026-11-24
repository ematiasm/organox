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
