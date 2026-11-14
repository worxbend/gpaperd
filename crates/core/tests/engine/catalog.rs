//! Catalog: identity, de-duplication by inode and validation state.

use gpaperd_core::catalog::{BadReason, Catalog, CatalogDelta, Check, FsChange, Origin};

use crate::support::{blob, catalog, file, files, found, id, linked, removed, version};

fn ids(catalog: &Catalog) -> Vec<&str> {
    catalog.items().map(|item| item.id.as_str()).collect()
}

#[test]
fn every_scanned_file_becomes_an_unchecked_item() {
    let catalog = catalog(&["/w/b.png", "/w/a.png"]);

    assert_eq!(ids(&catalog), ["/w/a.png", "/w/b.png"]);
    assert!(catalog.items().all(|item| item.check == Check::Unchecked));
}

#[test]
fn files_sharing_an_inode_count_once_under_the_smallest_id() {
    let hardlinks = vec![linked("/w/z-link.png", 7), linked("/w/a.png", 7)];

    let catalog = Catalog::from_local_files(hardlinks);

    assert_eq!(ids(&catalog), ["/w/a.png"]);
}

#[test]
fn deduplication_does_not_depend_on_scan_order() {
    let scan = vec![
        linked("/w/c.png", 1),
        linked("/w/b.png", 1),
        linked("/w/d.png", 2),
        file("/w/a.png"),
    ];
    let mut reversed = scan.clone();
    reversed.reverse();

    assert_eq!(
        Catalog::from_local_files(scan),
        Catalog::from_local_files(reversed)
    );
}

#[test]
fn a_file_listed_twice_is_one_item() {
    let catalog = catalog(&["/w/a.png", "/w/a.png"]);

    assert_eq!(catalog.len(), 1);
}

#[test]
fn a_bad_item_is_no_candidate() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png"]);

    catalog.set_check(&id("/w/a.png"), Check::Bad(BadReason::NotAnImage));

    let candidates: Vec<&str> = catalog.candidates().map(|item| item.id.as_str()).collect();
    assert_eq!(candidates, ["/w/b.png"]);
    assert!(!catalog.is_candidate(&id("/w/a.png")));
    assert!(catalog.is_candidate(&id("/w/b.png")));
}

#[test]
fn an_unchecked_and_a_usable_item_are_candidates() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png"]);

    catalog.set_check(&id("/w/a.png"), Check::Usable);

    assert_eq!(catalog.candidates().count(), 2);
}

#[test]
fn an_id_outside_the_catalog_is_no_candidate() {
    let catalog = catalog(&["/w/a.png"]);

    assert!(!catalog.is_candidate(&id("/w/gone.png")));
}

#[test]
fn a_check_for_an_unknown_id_is_ignored() {
    let mut catalog = catalog(&["/w/a.png"]);
    let before = catalog.clone();

    catalog.set_check(&id("/w/gone.png"), Check::Usable);

    assert_eq!(catalog, before);
}

/// A second path to the file at `path`.
fn hardlink(link: &str, path: &str) -> FsChange {
    FsChange::Found(linked(link, file(path).stamp.id.ino))
}

fn check_of(catalog: &Catalog, path: &str) -> Option<Check> {
    catalog.get(&id(path)).map(|item| item.check.clone())
}

#[test]
fn a_found_file_joins_the_catalog_unchecked() {
    let mut catalog = catalog(&["/w/a.png"]);

    let delta = catalog.apply(vec![found("/w/b.png")]);

    assert_eq!(ids(&catalog), ["/w/a.png", "/w/b.png"]);
    assert_eq!(check_of(&catalog, "/w/b.png"), Some(Check::Unchecked));
    assert_eq!(delta.added, [id("/w/b.png")]);
    assert!(delta.modified.is_empty());
}

#[test]
fn a_removed_file_leaves_the_catalog() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png"]);

    let delta = catalog.apply(vec![removed("/w/a.png")]);

    assert_eq!(ids(&catalog), ["/w/b.png"]);
    assert_eq!(delta, CatalogDelta::default());
}

#[test]
fn removing_an_unknown_path_changes_nothing() {
    let mut catalog = catalog(&["/w/a.png"]);
    let before = catalog.clone();

    let delta = catalog.apply(vec![removed("/w/gone.png")]);

    assert_eq!(catalog, before);
    assert_eq!(delta, CatalogDelta::default());
}

#[test]
fn changes_are_applied_in_order() {
    let mut created_then_deleted = catalog(&[]);
    let mut deleted_then_created = catalog(&["/w/a.png"]);

    created_then_deleted.apply(vec![found("/w/a.png"), removed("/w/a.png")]);
    deleted_then_created.apply(vec![removed("/w/a.png"), found("/w/a.png")]);

    assert!(created_then_deleted.is_empty());
    assert_eq!(ids(&deleted_then_created), ["/w/a.png"]);
}

#[test]
fn a_file_edited_in_place_loses_its_check() {
    let mut catalog = catalog(&["/w/a.png"]);
    catalog.set_check(&id("/w/a.png"), Check::Bad(BadReason::NotAnImage));

    let delta = catalog.apply(vec![FsChange::Found(version("/w/a.png", 1))]);

    assert_eq!(check_of(&catalog, "/w/a.png"), Some(Check::Unchecked));
    assert_eq!(delta.modified, [id("/w/a.png")]);
    assert!(delta.added.is_empty());
}

#[test]
fn a_file_replaced_by_an_atomic_save_is_the_same_item_on_a_new_inode() {
    let mut catalog = catalog(&["/w/a.png"]);
    catalog.set_check(&id("/w/a.png"), Check::Usable);
    let saved = linked("/w/a.png", 7);

    let delta = catalog.apply(vec![FsChange::Found(saved.clone())]);

    let item = catalog.get(&id("/w/a.png")).expect("the item is kept");
    assert_eq!(item.origin, Origin::Local(saved));
    assert_eq!(item.check, Check::Unchecked);
    assert_eq!(delta.modified, [id("/w/a.png")]);
}

#[test]
fn a_file_found_again_unchanged_keeps_its_check() {
    let mut catalog = catalog(&["/w/a.png"]);
    catalog.set_check(&id("/w/a.png"), Check::Bad(BadReason::NotAnImage));
    let before = catalog.clone();

    let delta = catalog.apply(vec![found("/w/a.png")]);

    assert_eq!(catalog, before);
    assert_eq!(delta, CatalogDelta::default());
}

#[test]
fn a_renamed_file_keeps_its_check() {
    let mut catalog = catalog(&["/w/old.png"]);
    catalog.set_check(&id("/w/old.png"), Check::Bad(BadReason::NotAnImage));

    catalog.apply(vec![
        removed("/w/old.png"),
        hardlink("/w/new.png", "/w/old.png"),
    ]);

    let bad = Check::Bad(BadReason::NotAnImage);
    assert_eq!(ids(&catalog), ["/w/new.png"]);
    assert_eq!(check_of(&catalog, "/w/new.png"), Some(bad));
}

#[test]
fn a_hardlink_found_later_is_no_second_item() {
    let mut catalog = catalog(&["/w/a.png"]);

    let delta = catalog.apply(vec![hardlink("/w/z-link.png", "/w/a.png")]);

    assert_eq!(ids(&catalog), ["/w/a.png"]);
    assert_eq!(delta, CatalogDelta::default());
}

#[test]
fn a_hardlink_takes_over_when_its_item_is_removed() {
    let mut catalog = catalog(&["/w/a.png"]);
    catalog.apply(vec![hardlink("/w/z-link.png", "/w/a.png")]);

    let delta = catalog.apply(vec![removed("/w/a.png")]);

    assert_eq!(ids(&catalog), ["/w/z-link.png"]);
    assert_eq!(delta.added, [id("/w/z-link.png")]);
}

#[test]
fn changes_lead_to_the_catalog_that_a_scan_of_the_same_files_builds() {
    let mut watched = catalog(&["/w/b.png", "/w/c.png"]);

    watched.apply(vec![
        hardlink("/w/a-link.png", "/w/b.png"),
        found("/w/d.png"),
        removed("/w/c.png"),
    ]);

    let scanned = Catalog::from_local_files(vec![
        file("/w/b.png"),
        linked("/w/a-link.png", file("/w/b.png").stamp.id.ino),
        file("/w/d.png"),
    ]);
    assert_eq!(ids(&watched), ["/w/a-link.png", "/w/d.png"]);
    assert_eq!(watched, scanned);
}

#[test]
fn a_scan_replaces_the_catalog() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png", "/w/c.png"]);
    let rescanned = vec![file("/w/a.png"), version("/w/b.png", 1), file("/w/d.png")];

    let delta = catalog.replace(rescanned);

    assert_eq!(ids(&catalog), ["/w/a.png", "/w/b.png", "/w/d.png"]);
    assert_eq!(delta.added, [id("/w/d.png")]);
    assert_eq!(delta.modified, [id("/w/b.png")]);
}

#[test]
fn a_scan_keeps_the_checks_of_unchanged_files() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png"]);
    catalog.set_check(&id("/w/a.png"), Check::Usable);
    catalog.set_check(&id("/w/b.png"), Check::Bad(BadReason::NotAnImage));

    catalog.replace(vec![file("/w/a.png"), version("/w/b.png", 1)]);

    assert_eq!(check_of(&catalog, "/w/a.png"), Some(Check::Usable));
    assert_eq!(check_of(&catalog, "/w/b.png"), Some(Check::Unchecked));
}

#[test]
fn a_path_scanned_twice_with_two_stamps_is_one_item_whatever_the_order() {
    let twice = vec![version("/w/a.png", 1), file("/w/a.png")];
    let mut reversed = twice.clone();
    reversed.reverse();

    assert_eq!(
        Catalog::from_local_files(twice),
        Catalog::from_local_files(reversed)
    );
    assert_eq!(
        catalog(&["/w/a.png", "/w/a.png"]),
        Catalog::from_local_files(files(&["/w/a.png"]))
    );
}

const DAWN: &str = "https://img.test/dawn.jpg";
const NOON: &str = "https://img.test/noon.jpg";

#[test]
fn a_cached_url_joins_the_catalog_unchecked_under_its_url() {
    let mut catalog = catalog(&["/w/a.png"]);

    let delta = catalog.set_remote(vec![blob(DAWN, 1)]);

    let item = catalog.get(&id(DAWN)).expect("the remote item is listed");
    assert_eq!(item.origin, Origin::Remote(blob(DAWN, 1)));
    assert_eq!(item.origin.path(), blob(DAWN, 1).blob);
    assert_eq!(item.check, Check::Unchecked);
    assert_eq!(delta.added, [id(DAWN)]);
}

#[test]
fn a_url_listed_twice_is_one_item() {
    let mut catalog = Catalog::default();

    catalog.set_remote(vec![blob(DAWN, 1), blob(DAWN, 1)]);

    assert_eq!(catalog.len(), 1);
}

#[test]
fn a_new_blob_of_a_url_drops_its_check() {
    let mut catalog = Catalog::default();
    catalog.set_remote(vec![blob(DAWN, 1)]);
    catalog.set_check(&id(DAWN), Check::Usable);

    let delta = catalog.set_remote(vec![blob(DAWN, 2)]);

    assert_eq!(check_of(&catalog, DAWN), Some(Check::Unchecked));
    assert_eq!(delta.modified, [id(DAWN)]);
}

#[test]
fn the_same_blob_again_keeps_its_check() {
    let mut catalog = Catalog::default();
    catalog.set_remote(vec![blob(DAWN, 1)]);
    catalog.set_check(&id(DAWN), Check::Usable);

    let delta = catalog.set_remote(vec![blob(DAWN, 1)]);

    assert_eq!(check_of(&catalog, DAWN), Some(Check::Usable));
    assert_eq!(delta, CatalogDelta::default());
}

#[test]
fn a_url_left_out_leaves_the_catalog() {
    let mut catalog = catalog(&["/w/a.png"]);
    catalog.set_remote(vec![blob(DAWN, 1), blob(NOON, 1)]);

    catalog.set_remote(vec![blob(NOON, 1)]);

    assert_eq!(ids(&catalog), ["/w/a.png", NOON]);
}

#[test]
fn local_changes_and_scans_keep_the_remote_items() {
    let mut catalog = catalog(&["/w/a.png"]);
    catalog.set_remote(vec![blob(DAWN, 1)]);
    catalog.set_check(&id(DAWN), Check::Usable);

    catalog.apply(vec![found("/w/b.png")]);
    catalog.replace(files(&["/w/c.png"]));

    assert_eq!(ids(&catalog), ["/w/c.png", DAWN]);
    assert_eq!(check_of(&catalog, DAWN), Some(Check::Usable));
}
