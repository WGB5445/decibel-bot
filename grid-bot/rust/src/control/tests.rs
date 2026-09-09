use super::*;

#[tokio::test]
async fn broadcasts_each_state_change_to_multiple_subscribers() {
    let handle = EngineHandle::new(EngineStatus::default());
    let mut first = handle.subscribe();
    let mut second = handle.subscribe();
    handle.update_status(|status| status.phase = "running".to_owned()).await;
    assert_eq!(first.recv().await.unwrap().phase, "running");
    assert_eq!(second.recv().await.unwrap().phase, "running");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn status_and_stop_use_single_line_json_protocol() {
    use std::path::PathBuf;
    let root = std::env::temp_dir().join(format!("decibel-grid-control-test-{}", std::process::id()));
    let socket = {
        #[cfg(unix)] { root.join("engine.sock") }
        #[cfg(windows)] {
            PathBuf::from(format!(r"\\.\pipe\decibel-grid-control-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
        }
    };
    let paths = ControlPaths { directory: root.clone(), socket, pid: root.join("engine.pid"), log: root.join("engine.log") };
    let handle = EngineHandle::new(EngineStatus { pid: 42, phase: "running".to_owned(), ..Default::default() });
    let server = start_server(&paths, handle.clone()).await.unwrap();
    match request(&paths, &Request::Status).await.unwrap() {
        Response::Status { status } => assert_eq!(status.pid, 42),
        response => panic!("unexpected response: {response:?}"),
    }
    match request(&paths, &Request::Stop { exit_mode: ExitMode::Hold }).await.unwrap() {
        Response::Accepted { .. } => {}
        response => panic!("unexpected response: {response:?}"),
    }
    assert!(handle.is_cancelled());
    server.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn control_paths_key_only_on_subaccount() {
    let paths = ControlPaths::for_subaccount("0x0abc").unwrap();
    assert!(paths.pid.ends_with("abc.pid"));
    #[cfg(unix)] assert!(paths.socket.ends_with("abc.sock"));
    #[cfg(windows)] assert_eq!(paths.socket.to_string_lossy(), r"\\.\pipe\grid-bot-abc");
}

#[test]
fn engine_status_round_trips_perp_fields() {
    let status = EngineStatus {
        perp_mode: Some("long".to_owned()), max_position: Some("0.01".to_owned()),
        position: Some("0.002".to_owned()), available_margin: Some("120".to_owned()),
        estimated_margin: Some("500".to_owned()), ..Default::default()
    };
    let encoded = serde_json::to_string(&status).unwrap();
    let decoded: EngineStatus = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.perp_mode, status.perp_mode);
    assert_eq!(decoded.max_position, status.max_position);
    assert_eq!(decoded.position, status.position);
    assert_eq!(decoded.available_margin, status.available_margin);
    assert_eq!(decoded.estimated_margin, status.estimated_margin);
}

#[test]
fn ladder_from_plan_formats_bid_and_ask_levels() {
    use crate::{GridLevel, GridPlan, LevelState, Side};
    use rust_decimal_macros::dec;
    let plan = GridPlan {
        mid: dec!(100), lower: dec!(90), upper: dec!(110), per_grid_base_size: None,
        bids: vec![GridLevel { side: Side::Bid, price: dec!(99), size: dec!(0.5), notional: dec!(49.5), state: LevelState::Planned }],
        asks: vec![GridLevel { side: Side::Ask, price: dec!(101), size: dec!(0.25), notional: dec!(25.25), state: LevelState::Selected }],
        quote_required: dec!(49.5), base_required: dec!(0.25), estimated_margin: None, ..Default::default()
    };
    let ladder = ladder_from_plan(&plan);
    assert_eq!(ladder.len(), 2);
    assert_eq!(ladder[0].side, "Bid"); assert_eq!(ladder[0].price, "99"); assert_eq!(ladder[0].size, "0.5"); assert_eq!(ladder[0].state, "Planned");
    assert_eq!(ladder[1].side, "Ask"); assert_eq!(ladder[1].price, "101"); assert_eq!(ladder[1].size, "0.25"); assert_eq!(ladder[1].state, "Selected");
}

#[test]
fn reconciliation_ladder_marks_resting_missing_and_unmanaged_levels() {
    use crate::{Side, reconcile::{ActualOrder, DesiredOrder, MatchedOrder, OrderOrigin, Reconciliation}};
    use rust_decimal_macros::dec;
    let bid = DesiredOrder { side: Side::Bid, price: dec!(99), size: dec!(1) };
    let ask = DesiredOrder { side: Side::Ask, price: dec!(101), size: dec!(1) };
    let reconciliation = Reconciliation {
        matched: vec![MatchedOrder { desired: bid.clone(), actual: ActualOrder { order_id: "bulk:1:Bid:0".to_owned(), side: Side::Bid, price: dec!(99), remaining_size: dec!(1), origin: OrderOrigin::Bulk } }],
        missing: vec![ask.clone()],
        unmanaged: vec![ActualOrder { order_id: "manual".to_owned(), side: Side::Ask, price: dec!(102), remaining_size: dec!(2), origin: OrderOrigin::Standalone }],
    };
    let ladder = ladder_from_reconciliation(&[bid, ask], &reconciliation);
    assert_eq!(ladder.len(), 3);
    assert_eq!(ladder[0].state, "Resting");
    assert_eq!(ladder[1].state, "Planned");
    assert_eq!(ladder[2].state, "Unmanaged");
}