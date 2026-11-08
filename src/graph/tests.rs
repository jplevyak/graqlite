use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use super::*;

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("graqlite-graph-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn prefix(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}."))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set(ids: &[u64]) -> IdSet {
    ids.iter().copied().collect()
}

fn graph() -> Graph {
    Graph::open(None, Options::default()).unwrap()
}

#[test]
fn value_interning() {
    let mut g = graph();
    let a = g.value(b"alice").unwrap();
    let b = g.value(b"bob").unwrap();
    assert_ne!(a, b);
    assert_eq!(g.value(b"alice").unwrap(), a);
    assert_eq!(g.find_value(b"bob"), Some(b));
    assert_eq!(g.find_value(b"carol"), None);
    assert_eq!(g.data(a).as_deref(), Some(&b"alice"[..]));
    // Empty data is a node, and nodes are not interned.
    let n = g.value(b"").unwrap();
    assert!(g.get(n).unwrap().is_node());
    assert_ne!(g.node().unwrap(), n);
    assert_eq!(g.data(n), None);
}

/// foo -> bar -> baz, by `prop`.
fn chain(g: &mut Graph) -> (u64, u64, u64, u64) {
    let prop = g.value(b"gloss").unwrap();
    let (foo, bar, baz) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
    g.link(foo, prop, bar).unwrap();
    g.link(bar, prop, baz).unwrap();
    (prop, foo, bar, baz)
}

#[test]
fn follow_links() {
    let mut g = graph();
    let (prop, foo, bar, baz) = chain(&mut g);
    assert_eq!(g.follow_forward(prop, [bar]), set(&[baz]));
    assert_eq!(g.follow_backward(prop, [bar]), set(&[foo]));
    assert_eq!(g.follow_forward(prop, [foo, bar]), set(&[bar, baz]));
    assert!(g.follow_forward(prop, [baz]).is_empty());
    let other = g.value(b"other").unwrap();
    assert!(g.follow_forward(other, [foo]).is_empty());
}

#[test]
fn link_dedup() {
    let mut g = graph();
    let (prop, foo, bar, _) = chain(&mut g);
    let l = g.find_link(foo, prop, bar).unwrap();
    // Re-asserting a live link returns it, whatever its other fields.
    assert_eq!(g.link(foo, prop, bar).unwrap(), l);
    let author = g.node().unwrap();
    let again = LinkFields {
        subject: foo,
        predicate: prop,
        object: bar,
        author,
        ..LinkFields::default()
    };
    assert_eq!(g.add_link(&again).unwrap(), l);
    assert_eq!(g.find_links(foo, prop, bar), set(&[l]));
}

#[test]
fn delete_and_recreate() {
    let mut g = graph();
    let (prop, foo, bar, _) = chain(&mut g);
    let l1 = g.find_link(foo, prop, bar).unwrap();

    let d1 = g.delete_link(foo, prop, bar).unwrap().unwrap();
    assert!(g.get(d1).unwrap().as_link().unwrap().dead());
    assert_eq!(g.find_link(foo, prop, bar), None);
    // Deleting again does nothing.
    assert_eq!(g.delete_link(foo, prop, bar).unwrap(), None);
    assert_eq!(g.find_links(foo, prop, bar), set(&[l1, d1]));
    assert!(g.follow_forward(prop, [foo]).is_empty());

    let l2 = g.link(foo, prop, bar).unwrap();
    assert!(l2 > d1);
    assert_eq!(g.find_link(foo, prop, bar), Some(l2));
    assert_eq!(g.follow_forward(prop, [foo]), set(&[bar]));

    let d2 = g.delete_link(foo, prop, bar).unwrap().unwrap();
    assert_eq!(g.find_link(foo, prop, bar), None);
    assert_eq!(g.find_links(foo, prop, bar), set(&[l1, d1, l2, d2]));
}

#[test]
fn write_dead() {
    // A link can be written dead to begin with.
    let mut g = graph();
    let p1 = g.value(b"p1").unwrap();
    let (foo, bar) = (g.node().unwrap(), g.node().unwrap());
    let dead = LinkFields {
        subject: foo,
        predicate: p1,
        object: bar,
        state: LinkState::Dead,
        ..LinkFields::default()
    };
    let l = g.add_link(&dead).unwrap();
    assert_eq!(g.add_link(&dead).unwrap(), l);
    assert!(g.get(l).unwrap().as_link().unwrap().dead());
    assert_eq!(g.find_link(foo, p1, bar), None);
    let all = g.links_at(Linkage::Predicate, [p1]);
    assert_eq!(all, set(&[l]));
    assert!(g.live(&all, None).is_empty());
}

#[test]
fn live_and_as_of() {
    // Several links with a predicate, all but one deleted.
    let mut g = graph();
    let p1 = g.value(b"p1").unwrap();
    let nodes: Vec<u64> = (0..3).map(|_| g.node().unwrap()).collect();
    let l0 = g.link(nodes[0], p1, nodes[1]).unwrap();
    let l1 = g.link(nodes[1], p1, nodes[2]).unwrap();
    let before = g.next_id() - 1;
    let d0 = g.delete_link(nodes[0], p1, nodes[1]).unwrap().unwrap();

    let all = g.links_at(Linkage::Predicate, [p1]);
    assert_eq!(all, set(&[l0, l1, d0]));
    assert_eq!(g.live(&all, None), set(&[l1]));
    // Before the delete, both were live.
    assert_eq!(g.live(&all, Some(before)), set(&[l0, l1]));
    assert_eq!(g.live(&all, Some(l0)), set(&[l0]));
    assert!(g.live(&all, Some(p1)).is_empty());

    assert_eq!(
        g.query_property([nodes[0]], Linkage::Subject, p1, Some(before))
            .unwrap(),
        (set(&[l0]), set(&[nodes[1]]))
    );
    assert_eq!(
        g.query_property([nodes[0]], Linkage::Subject, p1, None)
            .unwrap(),
        (IdSet::new(), IdSet::new())
    );
    assert!(matches!(
        g.query_property([nodes[0]], Linkage::Author, p1, None),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn entities_and_projection() {
    let mut g = graph();
    let name = g.value(b"name").unwrap();
    let knows = g.value(b"knows").unwrap();
    let (alice, bob, carol) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
    let alice_name = g.value(b"Alice").unwrap();
    let author = g.node().unwrap();
    let (sg1, sg2) = (g.node().unwrap(), g.node().unwrap());
    g.link(alice, name, alice_name).unwrap();
    let k1 = g
        .add_link(&LinkFields {
            subject: alice,
            predicate: knows,
            object: bob,
            author,
            when: 1000,
            subgraphs: vec![sg1, sg2],
            ..LinkFields::default()
        })
        .unwrap();
    let k2 = g.link(alice, knows, carol).unwrap();

    assert_eq!(
        g.entities_with(Linkage::Subject, alice, Linkage::Object, None),
        set(&[alice_name, bob, carol])
    );
    assert_eq!(
        g.entities_with(Linkage::Object, bob, Linkage::Subject, None),
        set(&[alice])
    );
    assert_eq!(
        g.entity_with(Linkage::Subject, alice, Linkage::Object, None),
        Some(carol)
    );
    assert_eq!(
        g.entity_with(Linkage::Subject, alice, Linkage::Object, Some(k1)),
        Some(bob)
    );
    assert_eq!(
        g.entity_with(Linkage::Subject, bob, Linkage::Object, None),
        None
    );

    let knows_links = g.links_at(Linkage::Predicate, [knows]);
    assert_eq!(knows_links, set(&[k1, k2]));
    assert_eq!(g.project(&knows_links, Linkage::Object), set(&[bob, carol]));
    assert_eq!(g.project(&knows_links, Linkage::Author), set(&[author]));
    assert_eq!(g.project(&knows_links, Linkage::Subgraph), set(&[sg1, sg2]));
    assert_eq!(
        g.project_map(&knows_links, Linkage::Object),
        BTreeMap::from([(k1, bob), (k2, carol)])
    );
    assert_eq!(g.links_at(Linkage::Subgraph, [sg2]), set(&[k1]));
    assert_eq!(g.links_at(Linkage::Author, [author]), set(&[k1]));
    assert!(g.links_at(Linkage::When, [1000]).is_empty());
}

#[test]
fn forward_reference() {
    let mut g = graph();
    let a = g.node().unwrap();
    let err = g.link(a, a, g.next_id());
    assert!(matches!(
        err,
        Err(Error::Record(crate::Error::ForwardReference))
    ));
}

#[test]
fn batch() {
    let dir = TempDir::new("batch");
    let prefix = dir.prefix("g");
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    let (prop, foo, bar, _) = g.batch(|g| Ok(chain(g))).unwrap();

    // A failed batch leaves nothing behind.
    let lost = g.next_id();
    let err = g.batch(|g| {
        let v = g.value(b"lost")?;
        g.link(foo, prop, v)?;
        g.delete_link(foo, prop, bar)?;
        Err::<(), _>(Error::InvalidArgument("give up"))
    });
    assert!(matches!(err, Err(Error::InvalidArgument("give up"))));
    assert_eq!(g.find_value(b"lost"), None);
    assert_eq!(g.follow_forward(prop, [foo]), set(&[bar]));
    assert!(g.links_at(Linkage::Subject, [foo]).len() == 1);
    // Its ids are not reused.
    let v = g.value(b"lost").unwrap();
    assert!(v > lost);
    g.commit().unwrap();
    drop(g);

    let g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.find_value(b"lost"), Some(v));
    assert_eq!(g.follow_forward(prop, [foo]), set(&[bar]));
}

#[test]
fn uncommitted_entries_are_lost() {
    let dir = TempDir::new("uncommitted");
    let prefix = dir.prefix("g");
    let (prop, foo, bar);
    {
        let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
        (prop, foo, bar, _) = chain(&mut g);
        g.commit().unwrap();
        let v = g.value(b"uncommitted").unwrap();
        g.link(foo, prop, v).unwrap();
        g.delete_link(foo, prop, bar).unwrap();
    }
    // The indexes still name the lost entries, but the graph ignores them.
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.find_value(b"uncommitted"), None);
    assert_eq!(g.links_at(Linkage::Subject, [foo]).len(), 1);
    assert_eq!(g.follow_forward(prop, [foo]), set(&[bar]));
    assert!(g.find_link(foo, prop, bar).is_some());
    let v = g.value(b"uncommitted").unwrap();
    assert_eq!(g.find_value(b"uncommitted"), Some(v));
    g.link(foo, prop, v).unwrap();
    assert_eq!(g.follow_forward(prop, [foo]), set(&[bar, v]));
}

#[test]
fn snapshot_restore() {
    let mut g = graph();
    let (prop, foo, bar, baz) = chain(&mut g);
    g.delete_link(bar, prop, baz).unwrap();
    let mut bytes = Vec::new();
    g.snapshot(&mut bytes).unwrap();

    let dir = TempDir::new("snapshot");
    let prefix = dir.prefix("g");
    let restored = Graph::restore(Some(&prefix), Options::default(), &mut &bytes[..]).unwrap();
    assert!(!restored.is_transient());
    assert_eq!(restored.next_id(), g.next_id());
    assert_eq!(restored.find_value(b"gloss"), Some(prop));
    assert_eq!(restored.follow_forward(prop, [foo]), set(&[bar]));
    assert!(restored.follow_forward(prop, [bar]).is_empty());
    assert_eq!(
        restored.records(restored.start_id()).count(),
        g.records(g.start_id()).count()
    );
}

#[test]
fn link_checks() {
    let mut g = graph();
    let (a, p) = (g.node().unwrap(), g.value(b"p").unwrap());
    for (s, p, o) in [(0, p, a), (a, 0, a), (a, p, 0)] {
        assert!(matches!(g.link(s, p, o), Err(Error::InvalidArgument(_))));
    }
    assert!(matches!(
        g.set_link(a, p, 0),
        Err(Error::InvalidArgument(_))
    ));
    // Links may be about links.
    let l = g.link(a, p, a).unwrap();
    let said = g.value(b"said").unwrap();
    let meta = g.link(l, said, a).unwrap();
    assert_eq!(g.follow_forward(said, [l]), set(&[a]));
    assert_eq!(g.follow_backward(said, [a]), set(&[l]));
    assert!(g.get(meta).unwrap().is_link());
}

#[test]
fn set_link() {
    let mut g = graph();
    let a = g.node().unwrap();
    let age = g.value(b"age").unwrap();
    let (v1, v2, v3) = (
        g.value(b"1").unwrap(),
        g.value(b"2").unwrap(),
        g.value(b"3").unwrap(),
    );
    assert_eq!(g.property(a, age), None);
    g.link(a, age, v1).unwrap();
    g.link(a, age, v2).unwrap();
    assert_eq!(g.follow_forward(age, [a]), set(&[v1, v2]));

    let l3 = g.set_link(a, age, v3).unwrap();
    assert_eq!(g.follow_forward(age, [a]), set(&[v3]));
    assert_eq!(g.property(a, age), Some(v3));
    // Setting the same value again changes nothing.
    let next = g.next_id();
    assert_eq!(g.set_link(a, age, v3).unwrap(), l3);
    assert_eq!(g.next_id(), next);
    // The old values are still in history.
    assert_eq!(g.find_link(a, age, v1), None);
    assert_eq!(g.find_links(a, age, v1).len(), 2);
}

#[test]
fn keys() {
    let mut g = graph();
    assert_eq!(g.find_by_key(b"alice"), None);
    let alice = g.node_by_key(b"alice").unwrap();
    assert_eq!(g.node_by_key(b"alice").unwrap(), alice);
    assert_eq!(g.find_by_key(b"alice"), Some(alice));
    assert_eq!(g.key(alice).as_deref(), Some(&b"alice"[..]));
    assert!(matches!(g.node_by_key(b""), Err(Error::InvalidArgument(_))));

    let bob = g.node_by_key(b"bob").unwrap();
    assert_ne!(bob, alice);
    assert!(matches!(g.set_key(bob, b"alice"), Err(Error::KeyInUse(id)) if id == alice));
    g.set_key(alice, b"alice").unwrap();

    // Renaming frees the old key.
    g.set_key(alice, b"alicia").unwrap();
    assert_eq!(g.key(alice).as_deref(), Some(&b"alicia"[..]));
    assert_eq!(g.find_by_key(b"alice"), None);
    assert_eq!(g.find_by_key(b"alicia"), Some(alice));
    g.set_key(bob, b"alice").unwrap();
    assert_eq!(g.find_by_key(b"alice"), Some(bob));

    // Any entity may have a key.
    let v = g.value(b"some value").unwrap();
    g.set_key(v, b"v").unwrap();
    assert_eq!(g.find_by_key(b"v"), Some(v));

    assert!(g.remove_key(bob).unwrap());
    assert!(!g.remove_key(bob).unwrap());
    assert_eq!(g.key(bob), None);
    assert_eq!(g.find_by_key(b"alice"), None);
    assert!(matches!(
        g.set_key(g.next_id(), b"x"),
        Err(Error::InvalidArgument(_))
    ));
}

/// Links in an import's numbering.
fn foreign_link(id: u64, s: u64, p: u64, o: u64) -> Record<'static> {
    Record::link(
        id,
        &LinkFields {
            subject: s,
            predicate: p,
            object: o,
            when: 1_700_000_000_000,
            ..LinkFields::default()
        },
    )
    .unwrap()
}

#[test]
fn import() {
    let mut g = graph();
    let knows = g.value(b"knows").unwrap();
    let local_alice = g.node().unwrap();

    // Foreign ids start well above the graph's.
    let base = 1000;
    let records = vec![
        Record::node(base),                // bob
        Record::value(base + 1, b"knows"), // already here
        Record::value(base + 2, b"likes"), // new
        Record::node(base + 3),            // unreferenced
        Record::value(base + 4, b"likes"), // a duplicate
        Record::value(base + 5, b"unreferenced"),
        foreign_link(base + 10, local_alice, base + 1, base),
        foreign_link(base + 20, base, base + 4, local_alice),
        foreign_link(base + 30, local_alice, base + 1, base), // a duplicate
        foreign_link(base + 40, base + 10, base + 2, base),   // about a link
    ];
    let imported = g.import(records, OnError::Stop).unwrap();
    assert!(imported.skipped.is_empty());
    let ids = &imported.ids;
    let bob = ids[&base];
    let likes = ids[&(base + 2)];
    assert_eq!(ids[&(base + 1)], knows);
    assert_eq!(ids[&(base + 4)], likes);
    assert!(!ids.contains_key(&(base + 3)));
    assert!(!ids.contains_key(&(base + 5)));
    assert_eq!(g.find_value(b"unreferenced"), None);
    assert_eq!(g.follow_forward(knows, [local_alice]), set(&[bob]));
    assert_eq!(g.follow_forward(likes, [bob]), set(&[local_alice]));
    assert_eq!(ids[&(base + 30)], ids[&(base + 10)]);
    assert_eq!(g.follow_forward(likes, [ids[&(base + 10)]]), set(&[bob]));
    assert_eq!(
        g.get(ids[&(base + 10)]).unwrap().as_link().unwrap().when(),
        1_700_000_000_000
    );

    // Re-importing changes nothing.
    let next = g.next_id();
    let again = vec![
        Record::node(base),
        Record::value(base + 1, b"knows"),
        foreign_link(base + 10, local_alice, base + 1, base),
    ];
    let imported = g.import(again, OnError::Stop).unwrap();
    // The node is new to this import, though: it is a different entity.
    assert_ne!(imported.ids[&base], bob);
    assert!(g.next_id() > next);
}

#[test]
fn import_errors() {
    let mut g = graph();
    let p = g.value(b"p").unwrap();
    let base = 1000;
    let bad = || {
        vec![
            Record::node(base),
            foreign_link(base + 10, base, p, base + 5), // not a record
            foreign_link(base + 20, base + 10, p, base), // refers to a skipped link
            foreign_link(base + 30, base, p, 900),      // a local id not in the graph
            foreign_link(base + 40, base, p, base),
        ]
    };

    let err = g.import(bad(), OnError::Stop);
    assert!(matches!(err, Err(Error::Unresolved(id)) if id == base + 5));

    let imported = g.import(bad(), OnError::Skip).unwrap();
    let skipped: Vec<_> = imported.skipped.iter().map(|(id, _)| *id).collect();
    assert_eq!(skipped, [base + 10, base + 20, base + 30]);
    assert!(matches!(imported.skipped[1].1, Error::Unresolved(id) if id == base + 10));
    let n = imported.ids[&base];
    assert_eq!(g.follow_forward(p, [n]), set(&[n]));

    let out_of_order = vec![Record::node(base + 1), Record::node(base)];
    assert!(matches!(
        g.import(out_of_order, OnError::Skip),
        Err(Error::InvalidArgument(_))
    ));

    // In a batch, a failed import leaves nothing.
    g.commit().unwrap();
    let links = g.links_at(Linkage::Predicate, [p]);
    let records = vec![
        Record::node(base),
        foreign_link(base + 10, base, p, base),
        foreign_link(base + 20, base, p, base + 5),
    ];
    let err = g.batch(|g| g.import(records, OnError::Stop));
    assert!(matches!(err, Err(Error::Unresolved(id)) if id == base + 5));
    assert_eq!(g.links_at(Linkage::Predicate, [p]), links);
}

#[test]
fn stats_and_counts() {
    let mut g = graph();
    let (a, b, c) = (g.node().unwrap(), g.node().unwrap(), g.node().unwrap());
    let (knows, likes) = (g.value(b"knows").unwrap(), g.value(b"likes").unwrap());
    g.link(a, knows, b).unwrap();
    g.link(a, knows, c).unwrap();
    g.link(b, knows, c).unwrap();
    g.link(a, likes, b).unwrap();
    g.link(a, likes, b).unwrap(); // a duplicate, not appended
    g.delete_link(a, knows, b).unwrap();

    let s = g.stats();
    let k = s.predicate(knows).unwrap();
    assert_eq!((k.links, k.dead, k.live()), (4, 1, 2));
    assert_eq!((k.subjects.len(), k.objects.len()), (2, 2));
    assert_eq!(k.per_subject(), 1.0);
    let l = s.predicate(likes).unwrap();
    assert_eq!((l.links, l.dead, l.live()), (1, 0, 1));
    assert!(s.predicate(a).is_none());
    assert_eq!((s.links(), s.dead(), s.live()), (5, 1, 3));
    assert_eq!(s.predicate_count(), 2);

    // Counts include dead links.
    assert_eq!(g.link_count(Linkage::Subject, a), 4);
    assert_eq!(g.link_count(Linkage::Predicate, knows), 4);
    assert_eq!(g.link_count(Linkage::Object, c), 2);
    assert_eq!(g.link_count(Linkage::When, a), 0);
    assert_eq!(g.property_count(a, Linkage::Subject, knows).unwrap(), 3);
    assert_eq!(g.property_count(c, Linkage::Object, knows).unwrap(), 2);
    assert_eq!(g.property_count(c, Linkage::Subject, knows).unwrap(), 0);
    assert!(g.property_count(a, Linkage::Author, knows).is_err());
}

#[test]
fn stats_rollback() {
    let mut g = graph();
    let (a, p) = (g.node().unwrap(), g.value(b"p").unwrap());
    g.link(a, p, a).unwrap();
    g.commit().unwrap();
    let before = g.stats().clone();
    let err = g.batch(|g| {
        let b = g.node()?;
        g.link(a, p, b)?;
        Err::<(), _>(Error::InvalidArgument("give up"))
    });
    assert!(err.is_err());
    assert_eq!(g.stats(), &before);
}

#[test]
fn stats_persist() {
    let dir = TempDir::new("stats");
    let prefix = dir.prefix("g");
    let build = |g: &mut Graph, n: u64| {
        let p = g.value(format!("p{}", n % 3).as_bytes()).unwrap();
        let (s, o) = (g.node().unwrap(), g.value(&n.to_le_bytes()).unwrap());
        g.link(s, p, o).unwrap();
    };

    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    (0..100).for_each(|n| build(&mut g, n));
    g.commit().unwrap();
    g.tick().unwrap();
    let old_file = fs::read(stats::path(&prefix)).unwrap();
    (100..200).for_each(|n| build(&mut g, n));
    g.commit().unwrap();
    let committed = g.stats().clone();
    assert_eq!(committed.links(), 200);
    // Uncommitted links are lost, and not counted.
    build(&mut g, 200);
    drop(g);

    let g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.stats(), &committed);
    drop(g);

    // Stale statistics are caught up.
    fs::write(stats::path(&prefix), &old_file).unwrap();
    let g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.stats(), &committed);
    drop(g);

    // Missing or invalid statistics are recounted.
    for contents in [
        None,
        Some(&b"garbage"[..]),
        Some(&old_file[..old_file.len() - 1]),
    ] {
        match contents {
            None => fs::remove_file(stats::path(&prefix)).unwrap(),
            Some(c) => fs::write(stats::path(&prefix), c).unwrap(),
        }
        let g = Graph::open(Some(&prefix), Options::default()).unwrap();
        assert_eq!(g.stats(), &committed);
    }

    // A restored graph counts its entries.
    let mut snapshot = Vec::new();
    Graph::open(Some(&prefix), Options::default())
        .unwrap()
        .snapshot(&mut snapshot)
        .unwrap();
    let restored = Graph::restore(None, Options::default(), &mut &snapshot[..]).unwrap();
    assert_eq!(restored.stats(), &committed);

    assert!(exists(&prefix));
    remove(&prefix).unwrap();
    assert!(!exists(&prefix));
    assert!(!stats::path(&prefix).exists());
}

#[test]
fn check_entry() {
    let mut g = Graph::open(None, Options::default()).unwrap();
    let lonely = g.node().unwrap();
    let keyed = g.node_by_key(b"/keyed").unwrap();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    let knows = g.value(b"knows").unwrap();
    let text = g.value(b"hello\0world, a longer value").unwrap();
    let link = g.link(a, knows, b).unwrap();
    let dead = g.delete_link(a, knows, b).unwrap().unwrap();
    for id in [keyed, a, b, knows, text, link, dead] {
        assert_eq!(g.check_entry(id), EntryCheck::Entry, "{id}");
    }
    assert_eq!(g.check_entry(lonely), EntryCheck::Unconfirmed);
    assert_eq!(g.check_entry(0), EntryCheck::NotEntry);
    assert_eq!(g.check_entry(g.next_id()), EntryCheck::NotEntry);
    // No id within an entry is confirmed as one.
    for id in [text, link, dead] {
        let len = g.get(id).unwrap().encoded_len();
        for inner in id + 1..id + len {
            assert_ne!(
                g.check_entry(inner),
                EntryCheck::Entry,
                "{id}+{}",
                inner - id
            );
        }
    }
    // The zero byte in the value reads as a node.
    assert_eq!(g.check_entry(text + 1 + 5), EntryCheck::Unconfirmed);
}

#[test]
fn commit_times() {
    let dir = TempDir::new("times");
    let prefix = dir.prefix("g");
    let first;
    {
        let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
        let p = g.value(b"p").unwrap();
        let (a, b) = (g.node().unwrap(), g.node().unwrap());
        first = g.link(a, p, b).unwrap();
        g.commit_at(1_000).unwrap();
        g.link(b, p, a).unwrap();
        // Rolled back: not a commit.
        g.rollback().unwrap();
        link_at_2000(&mut g, b, p, a);
        g.commit_at(2_000).unwrap();
        // Nothing appended: not a commit.
        g.commit_at(3_000).unwrap();
        assert_eq!(g.times().len(), 2);
    }
    let check = |g: &Graph| {
        assert_eq!(g.id_at_time(999), 0);
        assert_eq!(
            g.id_at_time(1_000),
            first + g.get(first).unwrap().encoded_len() - 1
        );
        assert_eq!(g.id_at_time(1_999), g.id_at_time(1_000));
        assert_eq!(g.id_at_time(u64::MAX), g.next_id() - 1);
        assert_eq!(g.time_of(first), Some(1_000));
        assert_eq!(g.time_of(g.next_id() - 1), Some(2_000));
        assert_eq!(g.time_of(g.next_id()), None);
    };
    // Read back from the times file.
    let g = Graph::open(Some(&prefix), Options::default()).unwrap();
    check(&g);
    drop(g);
    // A torn last record is ignored, and the commit rebuilt from its links.
    let path = times::path(&prefix);
    let len = fs::metadata(&path).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 5)
        .unwrap();
    let g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.times().len(), 2);
    assert_eq!(g.time_of(first), Some(1_000));
    drop(g);
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
}

/// Links `s p o` with the creation time 2 000.
fn link_at_2000(g: &mut Graph, s: u64, p: u64, o: u64) {
    g.add_link(&LinkFields {
        subject: s,
        predicate: p,
        object: o,
        when: 2_000,
        ..LinkFields::default()
    })
    .unwrap();
}

#[test]
fn commit_times_are_rebuilt_from_link_times() {
    let dir = TempDir::new("times-rebuilt");
    let prefix = dir.prefix("g");
    let link = |g: &mut Graph, when| {
        let p = g.value(b"p").unwrap();
        let (a, b) = (g.node().unwrap(), g.node().unwrap());
        g.add_link(&LinkFields {
            subject: a,
            predicate: p,
            object: b,
            when,
            ..LinkFields::default()
        })
        .unwrap()
    };
    let (early, late);
    {
        let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
        early = link(&mut g, 5_000);
        g.commit_at(5_000).unwrap();
        late = link(&mut g, 9_000);
        // Imported out of order: rebuilt times stay in order.
        link(&mut g, 7_000);
        g.commit_at(9_000).unwrap();
    }
    times::remove(&prefix).unwrap();
    let g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.time_of(early), Some(5_000));
    assert_eq!(g.time_of(late), Some(9_000));
    assert_eq!(g.time_of(g.next_id() - 1), Some(9_000));
    assert!(g.id_at_time(8_999) < late);
    assert!(times::path(&prefix).exists());
    // A snapshot has no times file: restoring rebuilds them.
    let mut snapshot = Vec::new();
    let mut g = g;
    g.snapshot(&mut snapshot).unwrap();
    let restored = Graph::restore(None, Options::default(), &mut &snapshot[..]).unwrap();
    assert_eq!(restored.time_of(early), Some(5_000));
    assert_eq!(restored.time_of(late), Some(9_000));
}

#[test]
fn large_entries_are_promoted_and_persist() {
    let dir = TempDir::new("large");
    let prefix = dir.prefix("g");
    let n = 3_000u64;
    let people;
    {
        let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
        let (kind, person, knows) = (
            g.value(b"type").unwrap(),
            g.value(b"person").unwrap(),
            g.value(b"knows").unwrap(),
        );
        let city = g.value(b"city").unwrap();
        let cities: Vec<u64> = (0..5)
            .map(|c| g.value(format!("c{c}").as_bytes()).unwrap())
            .collect();
        people = (0..n).map(|_| g.node().unwrap()).collect::<Vec<_>>();
        for (i, &p) in people.iter().enumerate() {
            g.link(p, kind, person).unwrap();
            g.link(p, city, cities[i % 5]).unwrap();
            if i > 0 {
                g.link(p, knows, people[0]).unwrap();
            }
        }
        // Deleting some of the links on a promoted entry.
        for &p in &people[1..11] {
            g.delete_link(p, knows, people[0]).unwrap();
        }
        g.commit().unwrap();
    }
    let check = |g: &Graph| {
        let kind = g.find_value(b"type").unwrap();
        let person = g.find_value(b"person").unwrap();
        let knows = g.find_value(b"knows").unwrap();
        let c0 = g.find_value(b"c0").unwrap();
        assert_eq!(g.link_count(Linkage::Predicate, kind), n);
        assert_eq!(g.property_count(person, Linkage::Object, kind).unwrap(), n);
        let typed = g.follow_backward(kind, [person]);
        assert_eq!(typed, people.iter().copied().collect());
        let in_c0 = g.follow_backward(g.find_value(b"city").unwrap(), [c0]);
        assert_eq!(in_c0.len() as u64, n / 5);
        // Everyone but the first knows them, less the ten deleted.
        let knowers = g.follow_backward(knows, [people[0]]);
        assert_eq!(knowers.len() as u64, n - 1 - 10);
        assert!(!knowers.contains(&people[5]));
        assert!(knowers.contains(&people[11]));
    };
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    check(&g);
    // A snapshot carries the promoted entries.
    let mut snapshot = Vec::new();
    g.snapshot(&mut snapshot).unwrap();
    let restored = Graph::restore(None, Options::default(), &mut &snapshot[..]).unwrap();
    check(&restored);
    // Appending to promoted entries after reopening.
    let kind = g.find_value(b"type").unwrap();
    let person = g.find_value(b"person").unwrap();
    let extra = g.node().unwrap();
    g.link(extra, kind, person).unwrap();
    assert_eq!(g.link_count(Linkage::Predicate, kind), n + 1);
    assert!(g.follow_backward(kind, [person]).contains(&extra));
}

#[test]
fn tick_compacts_to_the_history_kept() {
    let dir = TempDir::new("history");
    let prefix = dir.prefix("g");
    let day = 24 * 3600 * crate::record::LINK_TIME_PER_SECOND;
    let options = Options {
        history: Some(3 * day),
        ..Options::default()
    };
    let n = 1_000u64;
    let (city, cities, people, moved);
    {
        let mut g = Graph::open(Some(&prefix), options.clone()).unwrap();
        city = g.value(b"city").unwrap();
        cities = (0..5)
            .map(|c| g.value(format!("c{c}").as_bytes()).unwrap())
            .collect::<Vec<_>>();
        people = (0..n).map(|_| g.node().unwrap()).collect::<Vec<_>>();
        for (i, &p) in people.iter().enumerate() {
            g.link(p, city, cities[i % 5]).unwrap();
        }
        g.commit_at(now() - 10 * day).unwrap();
        // Everyone moves to the next city.
        for (i, &p) in people.iter().enumerate() {
            g.delete_link(p, city, cities[i % 5]).unwrap();
            g.link(p, city, cities[(i + 1) % 5]).unwrap();
        }
        g.commit_at(now() - 5 * day).unwrap();
        moved = g.next_id() - 1;
        // Recent history, which is kept.
        g.delete_link(people[0], city, cities[1]).unwrap();
        g.commit().unwrap();
        assert_eq!(g.link_count(Linkage::Predicate, city), 3 * n + 1);
        g.tick().unwrap();
        assert_eq!(g.horizon(), moved);
    }
    let check = |g: &Graph| {
        assert_eq!(g.horizon(), moved);
        // The first links and their deletions are gone from the entries.
        assert_eq!(g.link_count(Linkage::Predicate, city), n + 1);
        let in_c1 = g.follow_backward(city, [cities[1]]);
        assert_eq!(in_c1.len() as u64, n / 5 - 1);
        assert!(!in_c1.contains(&people[0]));
        let mut q = crate::query::Query::new();
        let p = q.var("p");
        q.pattern(p, city, cities[1]).as_of(moved);
        assert_eq!(q.run(g).unwrap().len() as u64, n / 5);
        q.as_of(moved - 1);
        assert!(matches!(q.plan(g), Err(Error::Compacted { .. })));
    };
    let mut g = Graph::open(Some(&prefix), options).unwrap();
    check(&g);
    // Nothing more to compact yet.
    g.tick().unwrap();
    check(&g);
    drop(g);
    // Without a history to keep, the horizon stays.
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    g.tick().unwrap();
    check(&g);
}

/// A small random number generator, for tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The facts live, and the subgraphs of each, as a model of a graph's.
type Model = BTreeMap<(u64, u64, u64), BTreeSet<u64>>;

/// Checks that `g`, as of `as_of`, has the facts and subgraphs of `model`.
fn check_subgraphs(
    g: &Graph,
    model: &Model,
    facts: &[(u64, u64, u64)],
    subgraphs: &[u64],
    as_of: Option<u64>,
) {
    for &(s, p, o) in facts {
        let expected: Vec<u64> = model
            .get(&(s, p, o))
            .map(|gs| gs.iter().copied().collect())
            .unwrap_or_default();
        let mut got = g.subgraphs_of(s, p, o, as_of);
        got.sort_unstable();
        assert_eq!(got, expected, "{s} {p} {o} as of {as_of:?}");
    }
    for &sg in subgraphs {
        let expected: BTreeSet<(u64, u64, u64)> = model
            .iter()
            .filter(|(_, gs)| gs.contains(&sg))
            .map(|(&f, _)| f)
            .collect();
        let got: BTreeSet<(u64, u64, u64)> = g
            .subgraph_links(sg, as_of)
            .into_iter()
            .map(|id| {
                assert!(g.in_subgraph(id, sg, as_of));
                let f = g.get(id).unwrap().as_link().unwrap().fields();
                (f.subject, f.predicate, f.object)
            })
            .collect();
        assert_eq!(got, expected, "subgraph {sg} as of {as_of:?}");
    }
}

#[test]
fn random_subgraphs() {
    // Adding to and removing from subgraphs, deleting and linking facts
    // again, against a model of each fact's subgraphs: through commits, a
    // rollback, reopening, a snapshot and compaction; and as of each commit.
    let dir = TempDir::new("subgraphs");
    let prefix = dir.prefix("g");
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    let mut rng = Rng(0x243f_6a88_85a3_08d3);
    let nodes: Vec<u64> = (0..4).map(|_| g.node().unwrap()).collect();
    let p = g.value(b"p").unwrap();
    let subgraphs: Vec<u64> = (0..3)
        .map(|i| g.value(format!("g{i}").as_bytes()).unwrap())
        .collect();
    let facts: Vec<(u64, u64, u64)> = nodes
        .iter()
        .flat_map(|&s| nodes.iter().map(move |&o| (s, p, o)))
        .collect();
    let mut model = Model::new();
    let mut history: Vec<(u64, Model)> = Vec::new();
    let change = |g: &mut Graph, model: &mut Model, rng: &mut Rng| {
        let (s, p, o) = facts[rng.below(facts.len())];
        let sg = subgraphs[rng.below(subgraphs.len())];
        let op = rng.below(5);
        match op {
            0 | 1 => {
                g.add_to_subgraph(s, p, o, sg).unwrap();
                model.entry((s, p, o)).or_default().insert(sg);
            }
            2 => {
                let removed = g.remove_from_subgraph(s, p, o, sg).unwrap();
                let was = model.get_mut(&(s, p, o)).is_some_and(|gs| gs.remove(&sg));
                assert_eq!(removed.is_some(), was);
            }
            3 => {
                g.delete_link(s, p, o).unwrap();
                model.remove(&(s, p, o));
            }
            _ => {
                // Linking a fact again keeps its subgraphs.
                g.link(s, p, o).unwrap();
                model.entry((s, p, o)).or_default();
            }
        }
    };
    for round in 0..30 {
        for _ in 0..10 {
            change(&mut g, &mut model, &mut rng);
            check_subgraphs(&g, &model, &facts, &subgraphs, None);
        }
        if round % 7 == 3 {
            // Changes rolled back: as at the commit before them.
            g.commit().unwrap();
            let mut lost = model.clone();
            for _ in 0..5 {
                change(&mut g, &mut lost, &mut rng);
            }
            g.rollback().unwrap();
            check_subgraphs(&g, &model, &facts, &subgraphs, None);
        }
        g.commit().unwrap();
        history.push((g.next_id() - 1, model.clone()));
        // The statistics count the facts live.
        assert_eq!(g.stats().live(), model.len() as u64, "round {round}");
    }
    for (id, then) in &history {
        check_subgraphs(&g, then, &facts, &subgraphs, Some(*id));
    }
    // Reopened, with its statistics counted again from the log.
    drop(g);
    fs::remove_file(stats::path(&prefix)).unwrap();
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    check_subgraphs(&g, &model, &facts, &subgraphs, None);
    assert_eq!(g.stats().live(), model.len() as u64);
    // A snapshot.
    let mut snapshot = Vec::new();
    g.snapshot(&mut snapshot).unwrap();
    let restored = Graph::restore(None, Options::default(), &mut &snapshot[..]).unwrap();
    check_subgraphs(&restored, &model, &facts, &subgraphs, None);
    assert_eq!(restored.stats(), g.stats());
    // Compacted to a horizon: as of it, and now, as before.
    let (horizon, then) = &history[history.len() / 2];
    g.compact(*horizon).unwrap();
    check_subgraphs(&g, then, &facts, &subgraphs, Some(*horizon));
    check_subgraphs(&g, &model, &facts, &subgraphs, None);
}

#[test]
fn subgraph_writes() {
    let mut g = graph();
    let (a, b, p) = (g.node().unwrap(), g.node().unwrap(), g.value(b"p").unwrap());
    let (g1, g2) = (g.value(b"g1").unwrap(), g.value(b"g2").unwrap());
    // Into a subgraph, a fact not live is linked; again, nothing changes.
    let l1 = g.add_to_subgraph(a, p, b, g1).unwrap();
    assert_eq!(g.find_link(a, p, b), Some(l1));
    assert_eq!(g.add_to_subgraph(a, p, b, g1).unwrap(), l1);
    // Into another, a new link with both; linking it plainly keeps them.
    let l2 = g.add_to_subgraph(a, p, b, g2).unwrap();
    assert_ne!(l2, l1);
    assert_eq!(g.subgraphs_of(a, p, b, None), [g1, g2]);
    assert_eq!(g.link(a, p, b).unwrap(), l2);
    assert!(!g.in_subgraph(l1, g1, None));
    assert!(g.in_subgraph(l1, g1, Some(l1)));
    // Out of one: the fact stays, in the other.
    let l3 = g.remove_from_subgraph(a, p, b, g1).unwrap().unwrap();
    assert_eq!(g.find_link(a, p, b), Some(l3));
    assert_eq!(g.subgraphs_of(a, p, b, None), [g2]);
    assert_eq!(g.remove_from_subgraph(a, p, b, g1).unwrap(), None);
    // Deleted: out of every one; linked again, in none.
    g.delete_link(a, p, b).unwrap();
    assert!(g.subgraphs_of(a, p, b, None).is_empty());
    assert!(g.subgraph_links(g2, None).is_empty());
    assert_eq!(g.subgraph_links(g2, Some(l3)), set(&[l3]));
    g.link(a, p, b).unwrap();
    assert!(g.subgraphs_of(a, p, b, None).is_empty());
    // One live fact throughout.
    assert_eq!(g.stats().live(), 1);
    assert!(g.add_to_subgraph(a, p, b, 0).is_err());
}

#[test]
fn standing_queries_and_subgraph_changes() {
    use crate::query::{Query, StandingQuery};
    let mut g = graph();
    let (a, b, p) = (g.node().unwrap(), g.node().unwrap(), g.value(b"p").unwrap());
    let sg = g.value(b"g").unwrap();
    let old = g.link(a, p, b).unwrap();
    g.commit().unwrap();
    let mut facts = Query::new();
    let (x, y) = (facts.var("x"), facts.var("y"));
    facts.pattern(x, p, y);
    let mut links = Query::new();
    let (l, x, y) = (links.var("l"), links.var("x"), links.var("y"));
    links.link_pattern(l, x, p, y);
    let mut sq_facts = StandingQuery::new(&facts, &g).unwrap();
    let mut sq_links = StandingQuery::new(&links, &g).unwrap();
    // A tag change: the fact is the same, its link is new.
    let new = g.add_to_subgraph(a, p, b, sg).unwrap();
    g.commit().unwrap();
    let changes = sq_facts.update(&g).unwrap();
    assert!(changes.is_empty(), "{changes:?}");
    let changes = sq_links.update(&g).unwrap();
    let rows = |c: &[(Vec<u64>, u64)]| c.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>();
    assert_eq!(rows(&changes.removed), [vec![old, a, b]]);
    assert_eq!(rows(&changes.added), [vec![new, a, b]]);
}

#[test]
fn ephemeral_values_and_keys() {
    let mut g = Graph::open(None, Options::default()).unwrap();
    let five = g.value(b"5").unwrap();
    let node = g.node_by_key(b"/a").unwrap();
    // The graph's own, if it has them.
    assert_eq!(g.ephemeral_value(b"5"), five);
    assert_eq!(g.ephemeral_key(b"/a"), node);
    // Else ids of their own, the same each time, that read back.
    let six = g.ephemeral_value(b"6");
    let b = g.ephemeral_key(b"/b");
    assert!(six & EPHEMERAL != 0 && b & EPHEMERAL != 0 && six != b);
    assert_eq!(g.ephemeral_value(b"6"), six);
    assert_eq!(g.ephemeral_key(b"/b"), b);
    // A value and a key of the same text are different.
    assert_ne!(g.ephemeral_key(b"6"), six);
    assert_eq!(g.data(six).as_deref(), Some(&b"6"[..]));
    assert_eq!(g.key(six), None);
    assert_eq!(g.key(b).as_deref(), Some(&b"/b"[..]));
    assert_eq!(g.data(b), None);
    assert_eq!(g.data(EPHEMERAL | 1000), None);
    // They aren't entries, and can't be written.
    assert!(g.find_value(b"6").is_none());
    assert!(g.link(node, five, six).is_err());
    // Read back while others are added: a copy, owned.
    let data = g.data(six).unwrap();
    assert!(matches!(data, std::borrow::Cow::Owned(_)));
    let held = g.ephemeral_count();
    assert_eq!(held, 3);
    for i in 0..100 {
        g.ephemeral_value(format!("x{i}").as_bytes());
    }
    assert_eq!(&*data, b"6");
    // Counted, with a reader's, which shares them.
    let r = g.reader();
    r.ephemeral_value(b"7");
    assert_eq!(g.ephemeral_count(), held + 101);
    // The graph's own data is borrowed.
    assert!(matches!(
        g.data(five),
        Some(std::borrow::Cow::Borrowed(b"5"))
    ));
}

#[test]
fn holds_free_ephemeral_ids_once_none_has_them() {
    let mut g = Graph::open(None, Options::default()).unwrap();
    let five = g.value(b"5").unwrap();
    let (a, b) = (g.ephemeral_hold(), g.ephemeral_hold());
    // The graph's own, as without a hold, and held by none.
    assert_eq!(a.value(&g, b"5"), five);
    // Holds share ids of the same data.
    let six = a.value(&g, b"6");
    assert_eq!(b.value(&g, b"6"), six);
    let k = a.key(&g, b"/k");
    let n = a.node(&g);
    assert_eq!(g.ephemeral_count(), 3);
    // Freed with the last hold of them: `b` keeps six, and a clone of `a`
    // its others.
    let a2 = a.clone();
    drop(a);
    assert_eq!(g.data(six).as_deref(), Some(&b"6"[..]));
    drop(a2);
    assert_eq!(g.ephemeral_count(), 1);
    assert_eq!(g.data(six).as_deref(), Some(&b"6"[..]));
    // Retained, by another hold.
    let c = g.ephemeral_hold();
    c.retain(six);
    c.retain(five);
    drop(b);
    assert_eq!(g.data(six).as_deref(), Some(&b"6"[..]));
    drop(c);
    assert_eq!(g.ephemeral_count(), 0);
    // Ids aren't given again: data held again has a new one.
    let d = g.ephemeral_hold();
    let again = d.value(&g, b"6");
    assert!(again != six && again != k && again != n);
    // One given without a hold is kept, even once held and let go.
    let seven = g.ephemeral_value(b"7");
    assert_eq!(d.value(&g, b"7"), seven);
    drop(d);
    assert_eq!(g.ephemeral_count(), 1);
    assert_eq!(g.data(seven).as_deref(), Some(&b"7"[..]));
    // A reader's holds are the graph's.
    let r = g.reader();
    let e = r.ephemeral_hold();
    let eight = e.value(&r, b"8");
    assert_eq!(g.data(eight).as_deref(), Some(&b"8"[..]));
    drop(e);
    assert_eq!(g.ephemeral_count(), 1);
}

/// A graph can be shared between threads: queries read `&Graph`.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Graph>();
    shared::<crate::query::Plan>();
    shared::<crate::query::Overlay>();
};

#[test]
fn queries_on_many_threads() {
    use crate::query::Query;
    use std::sync::RwLock;
    // Readers in parallel get what one thread gets, while a writer takes
    // turns with them through a lock.
    let mut g = Graph::open(None, Options::default()).unwrap();
    let knows = g.value(b"knows").unwrap();
    let nodes: Vec<u64> = (0..50).map(|_| g.node().unwrap()).collect();
    for i in 0..nodes.len() {
        g.link(nodes[i], knows, nodes[(i * 7 + 3) % nodes.len()])
            .unwrap();
    }
    g.commit().unwrap();
    let query = |g: &Graph| {
        let mut q = Query::new();
        let (a, b, c) = (q.var("a"), q.var("b"), q.var("c"));
        q.pattern(a, knows, b).pattern(b, knows, c);
        let mut rows = q.run(g).unwrap();
        rows.sort();
        rows
    };
    let expected = query(&g);
    let shared = RwLock::new(g);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                for _ in 0..20 {
                    let g = shared.read().unwrap();
                    let rows = query(&g);
                    // Each reader sees the graph whole: before the writer's
                    // links, or after some of them, never torn.
                    assert!(rows.len() >= expected.len());
                    assert!(expected.iter().all(|r| rows.contains(r)));
                }
            });
        }
        s.spawn(|| {
            for i in 0..10 {
                let mut g = shared.write().unwrap();
                g.link(nodes[i], knows, nodes[i + 1]).unwrap();
                g.commit().unwrap();
            }
        });
    });
    let g = shared.into_inner().unwrap();
    assert!(query(&g).len() > expected.len());
}

#[test]
fn readers() {
    use crate::query::Query;
    let mut g = Graph::open(None, Options::default()).unwrap();
    let knows = g.value(b"knows").unwrap();
    let [a, b, c] = [(); 3].map(|_| g.node().unwrap());
    g.link(a, knows, b).unwrap();
    g.commit().unwrap();
    let readers = g.readers();
    let first = readers.latest();
    // Uncommitted links aren't seen; committed ones are, by later readers.
    g.link(b, knows, c).unwrap();
    g.delete_link(a, knows, b).unwrap();
    let links = |g: &Graph| {
        let mut q = Query::new();
        let (x, y) = (q.var("x"), q.var("y"));
        q.pattern(x, knows, y);
        let mut rows = q.run(g).unwrap();
        rows.sort();
        rows
    };
    assert_eq!(links(&first), [[a, b]]);
    assert_eq!(links(&g.reader()), [[a, b]]);
    g.commit().unwrap();
    assert_eq!(links(&first), [[a, b]]);
    let second = readers.latest();
    assert_eq!(links(&second), [[b, c]]);
    assert_eq!(second.next_id(), g.next_id());
    assert!(first.next_id() < second.next_id());
    // A rollback's entries are never seen.
    g.link(c, knows, a).unwrap();
    g.rollback().unwrap();
    assert_eq!(links(&readers.latest()), [[b, c]]);
    // As of an earlier id, a reader sees the graph then.
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, knows, y).as_of(first.next_id() - 1);
    assert_eq!(q.run(&second).unwrap(), [[a, b]]);
    // A reader can't write, and has the commit's statistics.
    let mut r = g.reader();
    assert!(r.is_reader());
    assert!(r.link(a, knows, c).is_err());
    assert!(r.commit().is_err());
    assert!(r.rollback().is_err());
    r.tick().unwrap();
    assert_eq!(r.stats().links(), g.stats().links());
    // Values and keys the writer makes after are its own.
    let v = g.value(b"later").unwrap();
    assert_eq!(r.find_value(b"later"), None);
    assert!(r.data(v).is_none());
    // Ephemeral values are shared.
    assert_eq!(r.ephemeral_value(b"tmp"), g.ephemeral_value(b"tmp"));
}

#[test]
fn compaction_waits_for_readers() {
    use crate::query::Query;
    // A reader holds compaction back to its snapshot; dropped, it doesn't.
    let mut g = Graph::open(None, Options::default()).unwrap();
    let p = g.value(b"p").unwrap();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    g.link(a, p, b).unwrap();
    g.commit().unwrap();
    let r = g.reader();
    // Replaced after the reader's snapshot: history to the writer.
    g.delete_link(a, p, b).unwrap();
    g.link(a, p, b).unwrap();
    g.commit().unwrap();
    let horizon = g.next_id() - 1;
    g.compact(horizon).unwrap();
    assert!(g.horizon() < r.next_id());
    let mut q = Query::new();
    let (x, y) = (q.var("x"), q.var("y"));
    q.pattern(x, p, y);
    assert_eq!(q.run(&r).unwrap(), [[a, b]]);
    drop(r);
    g.compact(horizon).unwrap();
    assert_eq!(g.horizon(), horizon);
}

#[test]
fn vector_indexes_are_declared_checked_and_kept() {
    use crate::vector::{Metric, encode};
    let dir = TempDir::new("vector-indexes");
    let prefix = dir.prefix("g");
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    let p = g.value(b"embedding").unwrap();
    let (a, b) = (g.node().unwrap(), g.node().unwrap());
    let two = g.value(&encode(&[1.0, 0.0])).unwrap();
    let three = g.value(&encode(&[1.0, 0.0, 0.0])).unwrap();
    let text = g.value(b"[1, 0]").unwrap();
    g.link(a, p, two).unwrap();
    g.link(b, p, three).unwrap();
    // Refused while a live link's object isn't a vector of its dimension.
    let index = VectorIndex::new(p, 2, Metric::L2);
    assert!(matches!(
        g.create_vector_index(&index),
        Err(Error::NotAVector { value, dims: 2 }) if value == three
    ));
    assert!(g.vector_indexes().is_empty());
    g.delete_link(b, p, three).unwrap();
    assert!(g.create_vector_index(&index).unwrap());
    assert!(!g.create_vector_index(&index).unwrap());
    assert_eq!(g.vector_index(p), Some(&index));
    for bad in [
        VectorIndex::new(p, 0, Metric::L2),
        VectorIndex {
            m: 1,
            ..index.clone()
        },
    ] {
        assert!(matches!(
            g.create_vector_index(&bad),
            Err(Error::InvalidArgument(_))
        ));
    }
    // Live links must have a vector of the dimension; dead ones needn't.
    for bad in [three, text] {
        assert!(matches!(g.link(b, p, bad), Err(Error::NotAVector { .. })));
    }
    g.link(b, p, two).unwrap();
    g.delete_link(b, p, two).unwrap();
    g.commit().unwrap();
    // Replaced, then rolled back.
    let replaced = VectorIndex {
        m: 8,
        ef: 50,
        ..VectorIndex::new(p, 2, Metric::Dot)
    };
    assert!(g.create_vector_index(&replaced).unwrap());
    assert_eq!(g.vector_indexes(), std::slice::from_ref(&replaced));
    let r = g.reader();
    assert_eq!(r.vector_indexes(), std::slice::from_ref(&index));
    g.rollback().unwrap();
    assert_eq!(g.vector_indexes(), std::slice::from_ref(&index));
    drop(r);
    // Kept when the graph is opened again.
    drop(g);
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    assert_eq!(g.vector_indexes(), std::slice::from_ref(&index));
    // Its search is built again, of the live links' vectors.
    assert_eq!(g.vector_search(p).unwrap().hnsw.live_len(), 1);
    // And in a snapshot.
    let mut snapshot = Vec::new();
    g.snapshot(&mut snapshot).unwrap();
    let restored = Graph::restore(None, Options::default(), &mut &snapshot[..]).unwrap();
    assert_eq!(restored.vector_indexes(), std::slice::from_ref(&index));
    // Dropped, and then any object is linked.
    assert!(g.drop_vector_index(p).unwrap());
    assert!(!g.drop_vector_index(p).unwrap());
    assert!(g.vector_indexes().is_empty());
    g.link(b, p, text).unwrap();
}
