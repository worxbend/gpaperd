//! Rotation order: the shuffle bag and the alphabetical cursor.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use gpaperd_core::catalog::{BadReason, Catalog, Check, ItemId};
use gpaperd_core::ordering::{ShuffleBag, compare_ids, next_alphabetical};
use rand::SeedableRng;
use rand::rngs::SmallRng;

use crate::support::{catalog, id};

const FIVE: [&str; 5] = ["/w/a.png", "/w/b.png", "/w/c.png", "/w/d.png", "/w/e.png"];

fn rng(seed: u64) -> SmallRng {
    SmallRng::seed_from_u64(seed)
}

/// Draws `count` items, each time telling the bag what the last draw put on
/// screen.
fn draw_many(bag: &mut ShuffleBag, catalog: &Catalog, count: usize, seed: u64) -> Vec<ItemId> {
    let mut rng = rng(seed);
    let mut shown: Vec<ItemId> = Vec::new();
    for _ in 0..count {
        let next = bag
            .draw(catalog, shown.last(), &[], &mut rng)
            .expect("the catalog has usable items");
        shown.push(next);
    }
    shown
}

#[test]
fn a_round_shows_every_item_exactly_once() {
    let catalog = catalog(&FIVE);
    let everything: BTreeSet<ItemId> = FIVE.iter().map(|path| id(path)).collect();

    let shown = draw_many(&mut ShuffleBag::default(), &catalog, FIVE.len() * 50, 1);

    for round in shown.chunks(FIVE.len()) {
        let distinct: BTreeSet<ItemId> = round.iter().cloned().collect();
        assert_eq!(distinct, everything);
    }
}

#[test]
fn no_item_is_shown_twice_in_a_row_over_50_rounds() {
    let catalog = catalog(&FIVE[..3]);

    for seed in 0..20 {
        let shown = draw_many(&mut ShuffleBag::default(), &catalog, 3 * 50, seed);

        assert!(
            shown.windows(2).all(|pair| pair[0] != pair[1]),
            "seed {seed}"
        );
    }
}

#[test]
fn every_round_is_a_fresh_permutation() {
    let catalog = catalog(&FIVE);

    let shown = draw_many(&mut ShuffleBag::default(), &catalog, FIVE.len() * 20, 3);

    let orders: BTreeSet<&[ItemId]> = shown.chunks(FIVE.len()).collect();
    assert!(orders.len() > 1, "20 rounds all came out in the same order");
}

#[test]
fn a_restored_queue_continues_its_round() {
    let catalog = catalog(&FIVE);
    let queue = vec![id("/w/d.png"), id("/w/a.png"), id("/w/c.png")];
    let mut bag = ShuffleBag::from_queue(queue.clone());

    let shown = draw_many(&mut bag, &catalog, 3, 9);

    assert_eq!(shown, queue);
}

#[test]
fn an_id_that_left_the_catalog_is_skipped_when_reached() {
    let catalog = catalog(&["/w/a.png", "/w/b.png"]);
    let mut bag = ShuffleBag::from_queue(vec![id("/w/gone.png"), id("/w/b.png")]);

    let next = bag.draw(&catalog, None, &[], &mut rng(1));

    assert_eq!(next, Some(id("/w/b.png")));
}

#[test]
fn a_bad_item_is_skipped_when_reached() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png"]);
    catalog.set_check(&id("/w/a.png"), Check::Bad(BadReason::NotAnImage));
    let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png"), id("/w/b.png")]);

    let next = bag.draw(&catalog, None, &[], &mut rng(1));

    assert_eq!(next, Some(id("/w/b.png")));
}

#[test]
fn a_new_round_leaves_bad_items_out() {
    let mut catalog = catalog(&FIVE);
    catalog.set_check(&id("/w/c.png"), Check::Bad(BadReason::Missing));

    let shown = draw_many(&mut ShuffleBag::default(), &catalog, 40, 5);

    assert!(!shown.contains(&id("/w/c.png")));
}

#[test]
fn the_item_on_screen_is_not_drawn_while_another_is_usable() {
    let catalog = catalog(&["/w/a.png", "/w/b.png"]);
    let on_screen = id("/w/a.png");
    let mut bag = ShuffleBag::from_queue(vec![on_screen.clone(), id("/w/b.png")]);

    let next = bag.draw(&catalog, Some(&on_screen), &[], &mut rng(1));

    assert_eq!(next, Some(id("/w/b.png")));
}

#[test]
fn a_single_item_is_drawn_when_nothing_is_on_screen() {
    let catalog = catalog(&["/w/only.png"]);

    let next = ShuffleBag::default().draw(&catalog, None, &[], &mut rng(2));

    assert_eq!(next, Some(id("/w/only.png")));
}

#[test]
fn a_single_item_on_screen_leaves_nothing_to_draw() {
    let catalog = catalog(&["/w/only.png"]);
    let only = id("/w/only.png");
    let mut bag = ShuffleBag::default();

    for _ in 0..4 {
        assert_eq!(bag.draw(&catalog, Some(&only), &[], &mut rng(2)), None);
    }
}

#[test]
fn a_failed_item_is_skipped_when_reached() {
    let catalog = catalog(&FIVE);
    let failed = [id("/w/a.png")];
    let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png"), id("/w/b.png")]);

    let next = bag.draw(&catalog, None, &failed, &mut rng(1));

    assert_eq!(next, Some(id("/w/b.png")));
}

#[test]
fn the_item_on_screen_keeps_its_place_in_the_round_while_a_retry_passes_it() {
    let catalog = catalog(&["/w/a.png", "/w/b.png", "/w/c.png", "/w/d.png"]);
    let on_screen = id("/w/a.png");
    let failed = [id("/w/d.png")];
    // A round that started while a.png was on screen.
    let queue = ["/w/a.png", "/w/c.png", "/w/b.png"];
    let mut bag = ShuffleBag::from_queue(queue.iter().map(|path| id(path)).collect());

    let retry = bag.draw(&catalog, Some(&on_screen), &failed, &mut rng(1));

    assert_eq!(retry, Some(id("/w/c.png")));
    let rest: Vec<&ItemId> = bag.queue().collect();
    assert_eq!(rest, [&id("/w/a.png"), &id("/w/b.png")]);
}

#[test]
fn a_new_round_does_not_start_with_a_failed_item() {
    let catalog = catalog(&["/w/a.png", "/w/b.png", "/w/c.png"]);
    let on_screen = id("/w/a.png");
    let failed = [id("/w/b.png")];

    for seed in 0..20 {
        let next = ShuffleBag::default().draw(&catalog, Some(&on_screen), &failed, &mut rng(seed));

        assert_eq!(next, Some(id("/w/c.png")), "seed {seed}");
    }
}

#[test]
fn nothing_is_drawn_when_every_other_item_failed() {
    let catalog = catalog(&["/w/a.png", "/w/b.png"]);
    let on_screen = id("/w/a.png");
    let failed = [id("/w/b.png")];
    let mut bag = ShuffleBag::from_queue(vec![id("/w/b.png")]);

    assert_eq!(
        bag.draw(&catalog, Some(&on_screen), &failed, &mut rng(1)),
        None
    );
}

#[test]
fn an_empty_catalog_draws_nothing() {
    let mut bag = ShuffleBag::from_queue(vec![id("/w/gone.png")]);

    assert_eq!(bag.draw(&Catalog::default(), None, &[], &mut rng(1)), None);
}

#[test]
fn a_new_item_joins_the_round_in_progress() {
    let catalog = catalog(&FIVE);
    let mut positions = BTreeSet::new();

    for seed in 0..40 {
        let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png"), id("/w/b.png")]);
        bag.insert([id("/w/c.png")], &mut rng(seed));

        let round = draw_many(&mut bag, &catalog, 3, seed);

        let joined_at = round.iter().position(|shown| *shown == id("/w/c.png"));
        positions.insert(joined_at.expect("the new item is part of the round"));
    }

    assert_eq!(positions, BTreeSet::from([0, 1, 2]));
}

#[test]
fn an_item_already_queued_is_not_added_twice() {
    let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png"), id("/w/b.png")]);

    bag.insert([id("/w/a.png")], &mut rng(1));

    assert_eq!(bag.queue().count(), 2);
}

#[test]
fn a_new_item_waits_for_the_next_round_when_none_is_in_progress() {
    let mut bag = ShuffleBag::default();

    bag.insert([id("/w/a.png")], &mut rng(1));

    assert_eq!(bag.queue().count(), 0);
}

#[test]
fn several_new_items_join_the_round_once_each() {
    let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png")]);
    let new = [id("/w/b.png"), id("/w/c.png"), id("/w/b.png")];

    bag.insert(new, &mut rng(1));

    let queued: BTreeSet<&ItemId> = bag.queue().collect();
    assert_eq!(bag.queue().count(), 3);
    assert_eq!(queued.len(), 3);
}

#[test]
fn an_item_the_round_showed_does_not_join_it_again() {
    let mut bag = ShuffleBag::restore(vec![id("/w/b.png")], vec![id("/w/a.png")]);

    bag.insert([id("/w/a.png")], &mut rng(1));

    assert_eq!(bag.queue().collect::<Vec<_>>(), [&id("/w/b.png")]);
}

#[test]
fn a_cache_insert_with_no_newcomer_leaves_shown_unknown() {
    let cached = id("https://cdn.example/cached.jpg");
    let mut bag = ShuffleBag::restore_queue_only(vec![cached.clone(), id("/l/2.jpg")]);

    bag.insert([cached], &mut rng(1));

    assert!(!bag.knows_shown());
    assert_eq!(bag.queue().count(), 2);
}

#[test]
fn a_cache_load_before_the_scan_does_not_queue_what_the_round_showed() {
    let current = id("/l/3.jpg");
    let cached = id("https://cdn.example/extra.jpg");
    let mut bag = ShuffleBag::restore_queue_only(vec![id("/l/1.jpg"), id("/l/2.jpg")]);

    bag.insert([cached.clone()], &mut rng(1));

    assert!(!bag.knows_shown());
    bag.insert(
        [
            id("/l/1.jpg"),
            id("/l/2.jpg"),
            current.clone(),
            id("/l/4.jpg"),
        ],
        &mut rng(1),
    );

    assert!(bag.knows_shown());
    let queued: BTreeSet<ItemId> = bag.queue().cloned().collect();
    assert!(queued.contains(&cached));
    assert!(queued.contains(&id("/l/1.jpg")));
    assert!(queued.contains(&id("/l/2.jpg")));
    assert!(!queued.contains(&current));
    assert!(!queued.contains(&id("/l/4.jpg")));
    let shown: BTreeSet<ItemId> = bag.shown().cloned().collect();
    assert!(shown.contains(&current));
    assert!(shown.contains(&id("/l/4.jpg")));
}

#[test]
fn the_on_screen_id_omitted_from_the_scan_is_not_queued_again() {
    let current = id("/l/3.jpg");
    let mut bag = ShuffleBag::restore_queue_only(vec![id("/l/1.jpg"), id("/l/2.jpg")]);
    bag.insert([id("https://cdn.example/a.jpg")], &mut rng(1));
    bag.insert([id("/l/1.jpg"), id("/l/2.jpg"), id("/l/4.jpg")], &mut rng(1));
    let images = catalog(&["/l/1.jpg", "/l/2.jpg", "/l/3.jpg", "/l/4.jpg"]);

    bag.draw(&images, Some(&current), &[], &mut rng(1));
    bag.insert([current.clone()], &mut rng(2));

    assert!(bag.shown().any(|shown| shown == &current));
    assert!(bag.queue().all(|queued| queued != &current));
}

#[test]
fn a_scan_of_queued_files_still_ends_the_unknown_shown_state() {
    let mut bag = ShuffleBag::restore_queue_only(vec![id("/l/1.jpg"), id("/l/2.jpg")]);

    bag.insert([id("/l/1.jpg"), id("/l/2.jpg")], &mut rng(1));

    assert!(bag.knows_shown());
    bag.insert([id("/l/new.jpg")], &mut rng(1));
    assert_eq!(bag.queue().count(), 3);
}

#[test]
fn an_item_the_round_showed_is_not_put_back() {
    let mut bag = ShuffleBag::restore(vec![id("/w/b.png")], vec![id("/w/a.png")]);

    bag.put_back(id("/w/a.png"));

    assert_eq!(bag.queue().collect::<Vec<_>>(), [&id("/w/b.png")]);
}

#[test]
fn an_item_the_round_showed_is_skipped_when_reached() {
    let catalog = catalog(&FIVE);
    let queue = vec![id("/w/a.png"), id("/w/b.png")];
    let mut bag = ShuffleBag::restore(queue, vec![id("/w/a.png")]);

    let next = bag.draw(&catalog, None, &[], &mut rng(1));

    assert_eq!(next, Some(id("/w/b.png")));
}

#[test]
fn a_shown_item_is_recorded_once() {
    let mut bag = ShuffleBag::default();

    bag.mark_shown(&id("/w/a.png"));
    bag.mark_shown(&id("/w/b.png"));
    bag.mark_shown(&id("/w/a.png"));

    let shown: Vec<&ItemId> = bag.shown().collect();
    assert_eq!(shown, [&id("/w/a.png"), &id("/w/b.png")]);
}

#[test]
fn a_new_round_forgets_what_the_last_one_showed() {
    let catalog = catalog(&["/w/a.png", "/w/b.png"]);
    let last_round = vec![id("/w/a.png"), id("/w/b.png")];
    let mut bag = ShuffleBag::restore(Vec::new(), last_round);

    bag.draw(&catalog, Some(&id("/w/b.png")), &[], &mut rng(1));

    assert_eq!(bag.shown().count(), 0);
}

#[test]
fn an_item_put_back_is_drawn_next() {
    let catalog = catalog(&FIVE);
    let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png"), id("/w/b.png")]);
    let drawn = bag.draw(&catalog, None, &[], &mut rng(1));

    bag.put_back(id("/w/a.png"));

    assert_eq!(drawn, Some(id("/w/a.png")));
    assert_eq!(bag.draw(&catalog, None, &[], &mut rng(1)), drawn);
}

#[test]
fn an_item_put_back_is_not_queued_twice() {
    let mut bag = ShuffleBag::from_queue(vec![id("/w/a.png"), id("/w/b.png")]);

    bag.put_back(id("/w/b.png"));

    assert_eq!(bag.queue().count(), 2);
}

#[test]
fn file_names_sort_naturally() {
    assert_eq!(
        compare_ids(&id("/w/img2.png"), &id("/w/img10.png")),
        Ordering::Less
    );
}

#[test]
fn file_names_sort_without_regard_to_case() {
    assert_eq!(
        compare_ids(&id("/w/apple.png"), &id("/w/Banana.png")),
        Ordering::Less
    );
    assert_eq!(
        compare_ids(&id("/w/Cherry.png"), &id("/w/banana.png")),
        Ordering::Greater
    );
}

#[test]
fn a_url_sorts_by_its_last_path_segment() {
    let earlier = id("https://cdn.example/a/a.jpg?b=10");
    let later = id("https://cdn.example/z/a.jpg?b=2");

    // The last slash is first. The query or fragment of that segment is not
    // the name, so the whole id breaks the tie.
    assert_eq!(compare_ids(&earlier, &later), Ordering::Less);
    assert_eq!(
        compare_ids(
            &id("https://cdn.example/a/a.jpg#10"),
            &id("https://cdn.example/z/a.jpg#2"),
        ),
        Ordering::Less
    );
    // A `?` or `#` in a parent is not the name.
    assert_eq!(
        compare_ids(&id("/pics/?set/img10.png"), &id("/pics/?set/img2.png")),
        Ordering::Greater
    );
    assert_eq!(
        compare_ids(&id("/pics/#trip/img10.png"), &id("/pics/#trip/img2.png")),
        Ordering::Greater
    );
}

#[test]
fn only_the_file_name_decides_not_the_directory() {
    assert_eq!(
        compare_ids(&id("/zzz/a.png"), &id("/aaa/b.png")),
        Ordering::Less
    );
}

#[test]
fn equal_file_names_are_ordered_by_the_whole_id() {
    assert_eq!(
        compare_ids(&id("/one/a.png"), &id("/two/a.png")),
        Ordering::Less
    );
    assert_eq!(
        compare_ids(&id("/w/A.png"), &id("/w/a.png")),
        Ordering::Less
    );
    assert_eq!(
        compare_ids(&id("/w/a.png"), &id("/w/a.png")),
        Ordering::Equal
    );
}

#[test]
fn alphabetical_order_starts_at_the_first_item() {
    let catalog = catalog(&["/w/img10.png", "/w/img2.png", "/w/Img1.png"]);

    assert_eq!(next_alphabetical(&catalog, None), Some(id("/w/Img1.png")));
}

#[test]
fn alphabetical_order_walks_the_sorted_list_and_wraps() {
    let catalog = catalog(&["/w/img10.png", "/w/img2.png", "/w/Img1.png"]);
    let mut cursor = id("/w/Img1.png");
    let mut walked = Vec::new();

    for _ in 0..3 {
        cursor = next_alphabetical(&catalog, Some(&cursor)).expect("the catalog has items");
        walked.push(cursor.clone());
    }

    assert_eq!(
        walked,
        [id("/w/img2.png"), id("/w/img10.png"), id("/w/Img1.png")]
    );
}

#[test]
fn alphabetical_order_resumes_after_a_deleted_item() {
    let catalog = catalog(&["/w/a.png", "/w/c.png", "/w/d.png"]);
    let deleted = id("/w/b.png");

    assert_eq!(
        next_alphabetical(&catalog, Some(&deleted)),
        Some(id("/w/c.png"))
    );
}

#[test]
fn alphabetical_order_skips_bad_items() {
    let mut catalog = catalog(&["/w/a.png", "/w/b.png", "/w/c.png"]);
    catalog.set_check(&id("/w/b.png"), Check::Bad(BadReason::NotAnImage));

    assert_eq!(
        next_alphabetical(&catalog, Some(&id("/w/a.png"))),
        Some(id("/w/c.png"))
    );
}

#[test]
fn alphabetical_order_of_a_single_item_returns_it_again() {
    let catalog = catalog(&["/w/only.png"]);

    assert_eq!(
        next_alphabetical(&catalog, Some(&id("/w/only.png"))),
        Some(id("/w/only.png"))
    );
}

#[test]
fn alphabetical_order_of_an_empty_catalog_has_no_next() {
    assert_eq!(next_alphabetical(&Catalog::default(), None), None);
}
