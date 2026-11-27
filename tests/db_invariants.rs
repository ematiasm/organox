//! Database-layer immutability, foreign-key restriction, and ledger ceiling
//! trigger tests (#112).
//!
//! These tests exercise raw SQL statements directly against Postgres so the
//! database invariants are verified independently of Rust application guards.

use rust_decimal_macros::dec;
use sqlx::PgPool;
use uuid::Uuid;

fn db_error_code(err: &sqlx::Error) -> Option<String> {
    match err {
        sqlx::Error::Database(db_err) => db_err.code().map(|c| c.into_owned()),
        _ => None,
    }
}

async fn seed_product(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, name, price) VALUES ($1, 'product', $2, 100.00)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed product");
    id
}

async fn seed_customer(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO customers (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed customer");
    id
}

async fn seed_supplier(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO suppliers (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed supplier");
    id
}

async fn seed_wallet_and_method(
    pool: &PgPool,
    wallet_name: &str,
    method_name: &str,
) -> (Uuid, Uuid) {
    let wallet_id = Uuid::now_v7();
    sqlx::query("INSERT INTO wallets (id, name) VALUES ($1, $2)")
        .bind(wallet_id)
        .bind(wallet_name)
        .execute(pool)
        .await
        .expect("seed wallet");

    let method_id = Uuid::now_v7();
    sqlx::query("INSERT INTO payment_methods (id, name, wallet_id) VALUES ($1, $2, $3)")
        .bind(method_id)
        .bind(method_name)
        .bind(wallet_id)
        .execute(pool)
        .await
        .expect("seed payment method");

    (wallet_id, method_id)
}

async fn seed_completed_sale(pool: &PgPool, actor: Uuid, customer: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO sales (id, status, customer_id, customer_name, total, created_by, completed_by, completed_at)
         VALUES ($1, 'completed', $2, 'Customer', 100.00, $3, $3, now())",
    )
    .bind(id)
    .bind(customer)
    .bind(actor)
    .execute(pool)
    .await
    .expect("seed completed sale");
    id
}

#[sqlx::test]
async fn credit_notes_and_items_are_append_only_and_restrict_delete(pool: PgPool) {
    let actor = Uuid::now_v7();
    let product = seed_product(&pool, "Flour").await;
    let customer = seed_customer(&pool, "Alice").await;
    let sale = seed_completed_sale(&pool, actor, customer).await;

    let note_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_notes (id, sale_id, total, reason, created_by)
         VALUES ($1, $2, 50.00, 'Damaged unit', $3)",
    )
    .bind(note_id)
    .bind(sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed credit note");

    let item_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_note_items (id, credit_note_id, product_id, quantity, unit_price)
         VALUES ($1, $2, $3, 1, 50.00)",
    )
    .bind(item_id)
    .bind(note_id)
    .bind(product)
    .execute(&pool)
    .await
    .expect("seed credit note item");

    // 1. UPDATE and DELETE on credit_notes are rejected by append-only trigger.
    let err = sqlx::query("UPDATE credit_notes SET reason = 'Tampered' WHERE id = $1")
        .bind(note_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject updating credit_notes");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("DELETE FROM credit_notes WHERE id = $1")
        .bind(note_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject deleting credit_notes");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    // 2. UPDATE and DELETE on credit_note_items are rejected by append-only trigger.
    let err = sqlx::query("UPDATE credit_note_items SET quantity = 2 WHERE id = $1")
        .bind(item_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject updating credit_note_items");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("DELETE FROM credit_note_items WHERE id = $1")
        .bind(item_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject deleting credit_note_items");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    // 3. Even if the append-only trigger on credit_notes is temporarily disabled,
    //    ON DELETE RESTRICT prevents cascading deletion of credit_note_items.
    let mut tx = pool.begin().await.expect("begin tx");
    sqlx::query("ALTER TABLE credit_notes DISABLE TRIGGER credit_notes_no_update")
        .execute(&mut *tx)
        .await
        .expect("disable credit_notes_no_update");
    let err = sqlx::query("DELETE FROM credit_notes WHERE id = $1")
        .bind(note_id)
        .execute(&mut *tx)
        .await
        .expect_err("ON DELETE RESTRICT must reject deleting credit_notes with items");
    assert_eq!(
        db_error_code(&err).as_deref(),
        Some("23001"),
        "expected RESTRICT violation 23001, got: {err}"
    );
    tx.rollback().await.expect("rollback");
}

#[sqlx::test]
async fn closed_purchases_and_items_are_immutable_and_restrict_delete(pool: PgPool) {
    let actor = Uuid::now_v7();
    let flour = seed_product(&pool, "Flour").await;
    let sugar = seed_product(&pool, "Sugar").await;
    let supplier = seed_supplier(&pool, "Mill Co").await;

    // Seed a draft purchase, a completed purchase, and a cancelled purchase.
    let draft_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, created_by)
         VALUES ($1, 'draft', $2, $3)",
    )
    .bind(draft_id)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed draft purchase");

    let draft_item_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
         VALUES ($1, $2, $3, 2, 40.00)",
    )
    .bind(draft_item_id)
    .bind(draft_id)
    .bind(flour)
    .execute(&pool)
    .await
    .expect("seed draft purchase item");

    let completed_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, supplier_name, total, created_by, completed_by, completed_at)
         VALUES ($1, 'draft', $2, NULL, NULL, $3, NULL, NULL)",
    )
    .bind(completed_id)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed purchase to complete");

    let completed_item_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
         VALUES ($1, $2, $3, 3, 50.00)",
    )
    .bind(completed_item_id)
    .bind(completed_id)
    .bind(flour)
    .execute(&pool)
    .await
    .expect("seed item before completion");

    // Transitioning draft -> completed succeeds.
    sqlx::query(
        "UPDATE purchases
         SET status = 'completed', supplier_name = 'Mill Co', total = 150.00,
             completed_by = $2, completed_at = now()
         WHERE id = $1",
    )
    .bind(completed_id)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("completing draft purchase must succeed");

    let cancelled_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, created_by)
         VALUES ($1, 'cancelled', $2, $3)",
    )
    .bind(cancelled_id)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed cancelled purchase");

    // 1. Completed purchase cannot be updated or deleted.
    let err = sqlx::query("UPDATE purchases SET total = 999.00 WHERE id = $1")
        .bind(completed_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject updating a completed purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("DELETE FROM purchases WHERE id = $1")
        .bind(completed_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject deleting a completed purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("UPDATE purchases SET status = 'draft' WHERE id = $1")
        .bind(completed_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject reopening a completed purchase to draft");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    // ON CONFLICT DO UPDATE against an existing item of a completed purchase is also blocked.
    let err = sqlx::query(
        "INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
         VALUES ($1, $2, $3, 9, 50.00)
         ON CONFLICT (purchase_id, product_id) DO UPDATE SET quantity = EXCLUDED.quantity",
    )
    .bind(Uuid::now_v7())
    .bind(completed_id)
    .bind(flour)
    .execute(&pool)
    .await
    .expect_err("DB must reject upsert on completed purchase_items");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    // 2. Items of a completed purchase cannot be inserted, updated, deleted, or reparented.
    let err = sqlx::query(
        "INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
         VALUES ($1, $2, $3, 1, 20.00)",
    )
    .bind(Uuid::now_v7())
    .bind(completed_id)
    .bind(sugar)
    .execute(&pool)
    .await
    .expect_err("DB must reject inserting an item into a completed purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("UPDATE purchase_items SET quantity = 10 WHERE id = $1")
        .bind(completed_item_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject updating a completed purchase item");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("DELETE FROM purchase_items WHERE id = $1")
        .bind(completed_item_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject deleting a completed purchase item");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("UPDATE purchase_items SET purchase_id = $1 WHERE id = $2")
        .bind(completed_id)
        .bind(draft_item_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject moving a draft item into a completed purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("UPDATE purchase_items SET purchase_id = $1 WHERE id = $2")
        .bind(draft_id)
        .bind(completed_item_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject moving a completed item into a draft purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    // 3. Cancelled purchase and its items are also immutable.
    let err = sqlx::query("UPDATE purchases SET note = 'reopen' WHERE id = $1")
        .bind(cancelled_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject updating a cancelled purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query("DELETE FROM purchases WHERE id = $1")
        .bind(cancelled_id)
        .execute(&pool)
        .await
        .expect_err("DB must reject deleting a cancelled purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    let err = sqlx::query(
        "INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
         VALUES ($1, $2, $3, 1, 20.00)",
    )
    .bind(Uuid::now_v7())
    .bind(cancelled_id)
    .bind(sugar)
    .execute(&pool)
    .await
    .expect_err("DB must reject inserting an item into a cancelled purchase");
    assert!(
        err.to_string().contains("immutable"),
        "unexpected error: {err}"
    );

    // 4. Deleting a draft purchase with items is blocked by ON DELETE RESTRICT (23001);
    //    once its items are removed, deleting the draft succeeds.
    let err = sqlx::query("DELETE FROM purchases WHERE id = $1")
        .bind(draft_id)
        .execute(&pool)
        .await
        .expect_err("ON DELETE RESTRICT must block deleting a purchase that still has items");
    assert_eq!(
        db_error_code(&err).as_deref(),
        Some("23001"),
        "expected RESTRICT violation 23001, got: {err}"
    );

    sqlx::query("DELETE FROM purchase_items WHERE id = $1")
        .bind(draft_item_id)
        .execute(&pool)
        .await
        .expect("deleting draft purchase item must succeed");
    sqlx::query("DELETE FROM purchases WHERE id = $1")
        .bind(draft_id)
        .execute(&pool)
        .await
        .expect("deleting empty draft purchase must succeed");

    // 5. Non-existent purchase_id surfaces FK violation 23503.
    let err = sqlx::query(
        "INSERT INTO purchase_items (id, purchase_id, product_id, quantity, unit_cost)
         VALUES ($1, $2, $3, 1, 10.00)",
    )
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(flour)
    .execute(&pool)
    .await
    .expect_err("non-existent purchase_id must fail FK check");
    assert_eq!(
        db_error_code(&err).as_deref(),
        Some("23503"),
        "expected FK violation 23503, got: {err}"
    );
}

#[sqlx::test]
async fn customer_account_credit_note_ceiling_rejects_over_cancellation(pool: PgPool) {
    let actor = Uuid::now_v7();
    let customer = seed_customer(&pool, "Bob").await;
    let (wallet, method) = seed_wallet_and_method(&pool, "Cash", "Cash Method").await;
    let sale = seed_completed_sale(&pool, actor, customer).await;

    // Charge 100.00 to the customer's account for this sale.
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, created_by)
         VALUES ($1, $2, 'sale_charge', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(dec!(100.00))
    .bind(sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed sale_charge");

    // Collect a 60.00 receipt and allocate all 60.00 to the sale (outstanding = 40.00).
    let receipt_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, customer_id, created_by)
         VALUES ($1, $2, $3, 'account_payment', $4, $5, $6)",
    )
    .bind(receipt_wm)
    .bind(wallet)
    .bind(method)
    .bind(dec!(60.00))
    .bind(customer)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed receipt wallet movement");

    let receipt_entry = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'receipt', $3, $4, $5)",
    )
    .bind(receipt_entry)
    .bind(customer)
    .bind(dec!(-60.00))
    .bind(receipt_wm)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed receipt entry");

    sqlx::query(
        "INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(receipt_entry)
    .bind(sale)
    .bind(dec!(60.00))
    .execute(&pool)
    .await
    .expect("seed allocation");

    // Seed two credit notes referencing the sale.
    let note_over = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_notes (id, sale_id, total, reason, created_by)
         VALUES ($1, $2, 40.01, 'Over ceiling', $3)",
    )
    .bind(note_over)
    .bind(sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed over-ceiling note");

    // 1. Inserting a credit_note entry of -40.01 exceeds remaining 40.00 -> rejected at DB level.
    let err = sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, credit_note_id, created_by)
         VALUES ($1, $2, 'credit_note', $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(dec!(-40.01))
    .bind(sale)
    .bind(note_over)
    .bind(actor)
    .execute(&pool)
    .await
    .expect_err("DB must reject credit_note cancellation exceeding charged - allocated");
    assert!(
        err.to_string().contains("exceed the sale charge"),
        "unexpected error: {err}"
    );

    // 2. Inserting a credit_note entry of exactly -40.00 succeeds.
    let note_exact = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_notes (id, sale_id, total, reason, created_by)
         VALUES ($1, $2, 40.00, 'Exact ceiling', $3)",
    )
    .bind(note_exact)
    .bind(sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed exact-ceiling note");

    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, credit_note_id, created_by)
         VALUES ($1, $2, 'credit_note', $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(dec!(-40.00))
    .bind(sale)
    .bind(note_exact)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("exact credit_note cancellation up to charged - allocated must succeed");

    // 3. Any additional credit_note cancellation (-0.01) on the fully settled sale is rejected.
    let note_extra = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_notes (id, sale_id, total, reason, created_by)
         VALUES ($1, $2, 0.01, 'Extra penny', $3)",
    )
    .bind(note_extra)
    .bind(sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed extra note");

    let err = sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, credit_note_id, created_by)
         VALUES ($1, $2, 'credit_note', $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(dec!(-0.01))
    .bind(sale)
    .bind(note_extra)
    .bind(actor)
    .execute(&pool)
    .await
    .expect_err("DB must reject any credit_note cancellation once charged == cancelled + allocated");
    assert!(
        err.to_string().contains("exceed the sale charge"),
        "unexpected error: {err}"
    );

    // 4. A credit_note entry against a completed sale with no sale_charge (charged = 0) is rejected.
    let cash_sale = seed_completed_sale(&pool, actor, customer).await;
    let note_no_charge = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO credit_notes (id, sale_id, total, reason, created_by)
         VALUES ($1, $2, 10.00, 'Cash sale with no account charge', $3)",
    )
    .bind(note_no_charge)
    .bind(cash_sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed note for cash sale");

    let err = sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, credit_note_id, created_by)
         VALUES ($1, $2, 'credit_note', $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(dec!(-10.00))
    .bind(cash_sale)
    .bind(note_no_charge)
    .bind(actor)
    .execute(&pool)
    .await
    .expect_err("DB must reject credit_note cancellation when sale has no sale_charge");
    assert!(
        err.to_string().contains("exceed the sale charge"),
        "unexpected error: {err}"
    );
}

#[sqlx::test]
async fn account_unapplied_credit_cannot_go_negative_at_db_level(pool: PgPool) {
    let actor = Uuid::now_v7();
    let (wallet, method) = seed_wallet_and_method(&pool, "Main Cash", "Cash").await;

    // --- Customer side ---
    let customer = seed_customer(&pool, "Carol").await;
    let sale = seed_completed_sale(&pool, actor, customer).await;
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, sale_id, created_by)
         VALUES ($1, $2, 'sale_charge', 100.00, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(sale)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed sale_charge");

    let receipt_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, customer_id, created_by)
         VALUES ($1, $2, $3, 'account_payment', 50.00, $4, $5)",
    )
    .bind(receipt_wm)
    .bind(wallet)
    .bind(method)
    .bind(customer)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed customer receipt wm");

    let receipt_entry = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'receipt', -50.00, $3, $4)",
    )
    .bind(receipt_entry)
    .bind(customer)
    .bind(receipt_wm)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed customer receipt");

    // Allocating 50.01 against a 50.00 receipt is rejected.
    let err = sqlx::query(
        "INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
         VALUES ($1, $2, $3, 50.01)",
    )
    .bind(Uuid::now_v7())
    .bind(receipt_entry)
    .bind(sale)
    .execute(&pool)
    .await
    .expect_err("over-allocating a customer receipt must fail at DB level");
    assert!(
        err.to_string()
            .contains("exceed the money the receipt collected"),
        "unexpected error: {err}"
    );

    // Allocate 30.00 (20.00 unapplied remains).
    sqlx::query(
        "INSERT INTO customer_account_allocations (id, entry_id, sale_id, amount)
         VALUES ($1, $2, $3, 30.00)",
    )
    .bind(Uuid::now_v7())
    .bind(receipt_entry)
    .bind(sale)
    .execute(&pool)
    .await
    .expect("allocate 30.00");

    // Attempting to refund 20.01 (even with attribution) exceeds remaining 20.00 -> rejected.
    let mut tx = pool.begin().await.expect("begin customer over-refund tx");
    let refund_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, customer_id, created_by)
         VALUES ($1, $2, $3, 'account_payment_refund', 20.01, $4, $5)",
    )
    .bind(refund_wm)
    .bind(wallet)
    .bind(method)
    .bind(customer)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .expect("seed refund wm");
    let refund_entry = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'credit_refund', 20.01, $3, $4)",
    )
    .bind(refund_entry)
    .bind(customer)
    .bind(refund_wm)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .expect("insert refund entry");
    sqlx::query(
        "INSERT INTO customer_account_credit_refund_allocations (id, refund_entry_id, receipt_entry_id, amount)
         VALUES ($1, $2, $3, 20.01)",
    )
    .bind(Uuid::now_v7())
    .bind(refund_entry)
    .bind(receipt_entry)
    .execute(&mut *tx)
    .await
    .expect("insert refund attribution");
    let err = tx
        .commit()
        .await
        .expect_err("refunding past remaining unapplied credit must fail at commit");
    assert!(
        err.to_string()
            .contains("exceed the money the receipt collected"),
        "unexpected error: {err}"
    );

    // --- Supplier side ---
    let supplier = seed_supplier(&pool, "Grain Co").await;
    let purchase = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO purchases (id, status, supplier_id, supplier_name, total, created_by, completed_by, completed_at)
         VALUES ($1, 'completed', $2, 'Grain Co', 100.00, $3, $3, now())",
    )
    .bind(purchase)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed completed purchase");

    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, purchase_id, created_by)
         VALUES ($1, $2, 'purchase_charge', 100.00, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(supplier)
    .bind(purchase)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed purchase_charge");

    let pay_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, supplier_id, created_by)
         VALUES ($1, $2, $3, 'supplier_account_payment', 50.00, $4, $5)",
    )
    .bind(pay_wm)
    .bind(wallet)
    .bind(method)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed supplier payment wm");

    let pay_entry = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'payment', -50.00, $3, $4)",
    )
    .bind(pay_entry)
    .bind(supplier)
    .bind(pay_wm)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed supplier payment");

    // Allocate 30.00 (20.00 unapplied remains), then attempt to refund 20.01 -> rejected.
    sqlx::query(
        "INSERT INTO supplier_account_allocations (id, entry_id, purchase_id, amount)
         VALUES ($1, $2, $3, 30.00)",
    )
    .bind(Uuid::now_v7())
    .bind(pay_entry)
    .bind(purchase)
    .execute(&pool)
    .await
    .expect("allocate supplier 30.00");

    let mut tx = pool.begin().await.expect("begin supplier over-refund tx");
    let sup_refund_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, supplier_id, created_by)
         VALUES ($1, $2, $3, 'supplier_account_refund', 20.01, $4, $5)",
    )
    .bind(sup_refund_wm)
    .bind(wallet)
    .bind(method)
    .bind(supplier)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .expect("seed supplier refund wm");
    let sup_refund_entry = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'credit_refund', 20.01, $3, $4)",
    )
    .bind(sup_refund_entry)
    .bind(supplier)
    .bind(sup_refund_wm)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .expect("insert supplier refund entry");
    sqlx::query(
        "INSERT INTO supplier_account_credit_refund_allocations (id, refund_entry_id, source_entry_id, amount)
         VALUES ($1, $2, $3, 20.01)",
    )
    .bind(Uuid::now_v7())
    .bind(sup_refund_entry)
    .bind(pay_entry)
    .execute(&mut *tx)
    .await
    .expect("insert supplier refund attribution");
    let err = tx
        .commit()
        .await
        .expect_err("supplier refund past remaining unapplied credit must fail at commit");
    assert!(
        err.to_string().contains("exceed"),
        "unexpected error: {err}"
    );

    // Direct unattributed credit_refund (no attribution rows) is rejected on both ledgers.
    let unattr_cust_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, customer_id, created_by)
         VALUES ($1, $2, $3, 'account_payment_refund', 10.00, $4, $5)",
    )
    .bind(unattr_cust_wm)
    .bind(wallet)
    .bind(method)
    .bind(customer)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed unattributed customer refund wm");
    let err = sqlx::query(
        "INSERT INTO customer_account_entries (id, customer_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'credit_refund', 10.00, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(customer)
    .bind(unattr_cust_wm)
    .bind(actor)
    .execute(&pool)
    .await
    .expect_err("unattributed customer credit_refund must fail at DB level");
    assert!(
        err.to_string().contains("attributed"),
        "unexpected error: {err}"
    );

    let unattr_sup_wm = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO wallet_movements (id, wallet_id, payment_method_id, kind, amount, supplier_id, created_by)
         VALUES ($1, $2, $3, 'supplier_account_refund', 10.00, $4, $5)",
    )
    .bind(unattr_sup_wm)
    .bind(wallet)
    .bind(method)
    .bind(supplier)
    .bind(actor)
    .execute(&pool)
    .await
    .expect("seed unattributed supplier refund wm");
    let err = sqlx::query(
        "INSERT INTO supplier_account_entries (id, supplier_id, kind, amount, wallet_movement_id, created_by)
         VALUES ($1, $2, 'credit_refund', 10.00, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(supplier)
    .bind(unattr_sup_wm)
    .bind(actor)
    .execute(&pool)
    .await
    .expect_err("unattributed supplier credit_refund must fail at DB level");
    assert!(
        err.to_string().contains("attributed"),
        "unexpected error: {err}"
    );
}
