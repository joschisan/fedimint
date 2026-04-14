use tokio::sync::oneshot;

use super::mem_impl::MemDatabase;
use super::{
    Database, IRawDatabaseExt, IWriteDatabaseTransactionOpsTyped, TestKey, TestVal,
    future_returns_shortly,
};
use crate::runtime::spawn;

async fn waiter(db: &Database, key: TestKey) -> tokio::task::JoinHandle<TestVal> {
    let db = db.clone();
    let (tx, rx) = oneshot::channel::<()>();
    let join_handle = spawn("wait key exists", async move {
        let sub = db.wait_key_exists(&key);
        tx.send(()).unwrap();
        sub.await
    });
    rx.await.unwrap();
    join_handle
}

#[tokio::test]
async fn test_wait_key_before_transaction() {
    let key = TestKey(1);
    let val = TestVal(2);
    let db = MemDatabase::new().into_database();

    let key_task = waiter(&db, TestKey(1)).await;

    let mut tx = db.begin_write_transaction().await;
    tx.insert_new_entry(&key, &val).await;
    tx.commit_tx().await;

    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        Some(TestVal(2)),
        "should notify"
    );
}

#[tokio::test]
async fn test_wait_key_before_insert() {
    let key = TestKey(1);
    let val = TestVal(2);
    let db = MemDatabase::new().into_database();

    let mut tx = db.begin_write_transaction().await;
    let key_task = waiter(&db, TestKey(1)).await;
    tx.insert_new_entry(&key, &val).await;
    tx.commit_tx().await;

    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        Some(TestVal(2)),
        "should notify"
    );
}

#[tokio::test]
async fn test_wait_key_after_insert() {
    let key = TestKey(1);
    let val = TestVal(2);
    let db = MemDatabase::new().into_database();

    let mut tx = db.begin_write_transaction().await;
    tx.insert_new_entry(&key, &val).await;

    let key_task = waiter(&db, TestKey(1)).await;

    tx.commit_tx().await;

    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        Some(TestVal(2)),
        "should notify"
    );
}

#[tokio::test]
async fn test_wait_key_after_commit() {
    let key = TestKey(1);
    let val = TestVal(2);
    let db = MemDatabase::new().into_database();

    let mut tx = db.begin_write_transaction().await;
    tx.insert_new_entry(&key, &val).await;
    tx.commit_tx().await;

    let key_task = waiter(&db, TestKey(1)).await;
    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        Some(TestVal(2)),
        "should notify"
    );
}

#[tokio::test]
async fn test_wait_key_isolated_db() {
    let module_instance_id = 10;
    let key = TestKey(1);
    let val = TestVal(2);
    let db = MemDatabase::new().into_database();
    let db = db.with_prefix_module_id(module_instance_id);

    let key_task = waiter(&db, TestKey(1)).await;

    let mut tx = db.begin_write_transaction().await;
    tx.insert_new_entry(&key, &val).await;
    tx.commit_tx().await;

    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        Some(TestVal(2)),
        "should notify"
    );
}

#[tokio::test]
async fn test_wait_key_isolated_tx() {
    let module_instance_id = 10;
    let key = TestKey(1);
    let val = TestVal(2);
    let db = MemDatabase::new().into_database();

    let key_task = waiter(&db.with_prefix_module_id(module_instance_id), TestKey(1)).await;

    let mut tx = db.begin_write_transaction().await;
    let mut tx_mod = tx.to_ref_with_prefix_module_id(module_instance_id);
    tx_mod.insert_new_entry(&key, &val).await;
    drop(tx_mod);
    tx.commit_tx().await;

    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        Some(TestVal(2)),
        "should notify"
    );
}

#[tokio::test]
async fn test_wait_key_no_transaction() {
    let db = MemDatabase::new().into_database();

    let key_task = waiter(&db, TestKey(1)).await;
    assert_eq!(
        future_returns_shortly(async { key_task.await.unwrap() }).await,
        None,
        "should not notify"
    );
}
