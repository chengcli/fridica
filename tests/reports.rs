use chrono::NaiveDate;
use chrono_tz::America::Detroit;
use fridica::{report, store::Store};

#[test]
fn local_days_follow_both_dst_transitions() {
    let spring = report::day_bounds(NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(), Detroit).unwrap();
    let autumn =
        report::day_bounds(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), Detroit).unwrap();
    assert_eq!(spring.1 - spring.0, 23. * 3600.);
    assert_eq!(autumn.1 - autumn.0, 25. * 3600.);
}

#[tokio::test]
async fn regenerate_and_recover_export_without_duplicate_posts() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("state.sqlite3")).await.unwrap();
    let day = NaiveDate::from_ymd_opt(2026, 9, 26).unwrap();
    report::generate(&s, "CROOM".into(), day, Detroit, 1.)
        .await
        .unwrap();
    let reports = dir.path().join("reports");
    assert_eq!(
        report::export_pending(&s, reports.clone()).await.unwrap(),
        1
    );
    let target = reports.join("2026-09-26-CROOM.md");
    assert!(target.exists());
    assert!(report::queue_post(
        &s,
        "CROOM".into(),
        day.to_string(),
        "TTEAM:CROOM:daily".into(),
        1.
    )
    .await
    .unwrap());
    report::generate(&s, "CROOM".into(), day, Detroit, 2.)
        .await
        .unwrap();
    assert!(!report::queue_post(
        &s,
        "CROOM".into(),
        day.to_string(),
        "TTEAM:CROOM:daily".into(),
        2.
    )
    .await
    .unwrap());
    // File landed, database acknowledgement was lost: atomically rewrite it.
    std::fs::write(&target, "partial old export").unwrap();
    assert_eq!(
        report::export_pending(&s, reports.clone()).await.unwrap(),
        1
    );
    assert!(std::fs::read_to_string(target)
        .unwrap()
        .contains("Timezone: America/Detroit"));
    assert_eq!(report::export_pending(&s, reports).await.unwrap(), 0);
}
