use super::*;
use crate::graph::Options;

fn graph() -> Graph {
    Graph::open(None, Options::default()).unwrap()
}

fn tokens(text: &str) -> Vec<Token> {
    lex(text).unwrap().into_iter().map(|t| t.token).collect()
}

fn rows(out: &Output) -> &[Vec<u64>] {
    match out {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

/// Runs one statement.
fn one(g: &mut Graph, text: &str) -> Output {
    let mut out = execute(g, text).unwrap();
    assert_eq!(out.len(), 1);
    out.pop().unwrap()
}

/// The data of the values in each row.
fn data(g: &Graph, rows: &[Vec<u64>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|&id| {
                    String::from_utf8(g.data(id).as_deref().unwrap_or(b"-").to_vec()).unwrap()
                })
                .collect()
        })
        .collect()
}

#[test]
fn lexing() {
    assert_eq!(
        tokens("SELECT ?x, ? where /a/b knows _:n . -- comment\n \"a\\\"b\" 12 3.5 0x1f"),
        [
            Token::Keyword("select"),
            Token::Var("x".into()),
            Token::Punct(","),
            Token::Var("".into()),
            Token::Keyword("where"),
            Token::Key("/a/b".into()),
            Token::Ident("knows".into()),
            Token::Label("n".into()),
            Token::Punct("."),
            Token::Text("a\"b".into()),
            Token::Number("12".into()),
            Token::Number("3.5".into()),
            Token::Number("0x1f".into()),
            Token::End,
        ]
    );
    assert_eq!(
        tokens("a/* x */<= != ++ / [ ] ;"),
        [
            Token::Ident("a".into()),
            Token::Punct("<="),
            Token::Punct("!="),
            Token::Punct("++"),
            Token::Punct("/"),
            Token::Punct("["),
            Token::Punct("]"),
            Token::Punct(";"),
            Token::End,
        ]
    );
    // Positions.
    let t = lex("select\n  ?x").unwrap();
    assert_eq!((t[1].line, t[1].column), (2, 3));
    assert!(matches!(
        lex("select \"open"),
        Err(Error::Syntax {
            line: 1,
            column: 8,
            ..
        })
    ));
    assert!(matches!(lex("/* open"), Err(Error::Syntax { .. })));
    assert!(matches!(
        lex("select $"),
        Err(Error::Syntax { column: 8, .. })
    ));
}

#[test]
fn parsing() {
    let v = |s: &str| Node::Var(s.into());
    let val = |s: &str| Node::Value(s.into());
    let s = parse("select ?x where ?x a person ; knows ?y, /bob . ?y age 3").unwrap();
    assert_eq!(
        s,
        [Statement::Select {
            distinct: false,
            vars: Some(vec![SelectItem::Var("x".into())]),
            having: vec![],
            by: vec![],
            pattern: Pattern::And(vec![
                Pattern::And(vec![
                    Pattern::Triple(v("x"), val("type"), val("person")),
                    Pattern::Triple(v("x"), val("knows"), v("y")),
                    Pattern::Triple(v("x"), val("knows"), Node::Key("/bob".into())),
                ]),
                Pattern::And(vec![Pattern::Triple(v("y"), val("age"), val("3"))]),
            ]),
            limit: None,
            offset: None,
            as_of: None,
        }]
    );
    // Blank nodes, named and not.
    let s = parse("verify ?x knows [ :?y age 3 ] ; likes [ name \"z\" ]").unwrap();
    let Statement::Verify {
        pattern: Pattern::And(subjects),
        ..
    } = &s[0]
    else {
        panic!("{s:?}")
    };
    let Pattern::And(ps) = &subjects[0] else {
        panic!()
    };
    assert_eq!(ps[0], Pattern::Triple(v("y"), val("age"), val("3")));
    assert_eq!(ps[1], Pattern::Triple(v("x"), val("knows"), v("y")));
    assert!(matches!(&ps[3], Pattern::Triple(_, p, Node::Blank(_)) if *p == val("likes")));
    // Several statements.
    assert_eq!(
        parse("verify ?x p ?y verify ?y q ?z quit select")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn statements() {
    let mut g = graph();
    let out = one(
        &mut g,
        r#"insert /alice a person ; name "Alice" ; age 30 ; knows /bob, /carol .
                  /bob a person ; name "Bob" ; age 25 ; knows /carol .
                  /carol a person ; name "Carol" ; age 35 ; banned true"#,
    );
    assert_eq!(out, Output::Inserted(13));
    // Inserting again adds nothing.
    assert_eq!(one(&mut g, "insert /alice knows /bob"), Output::Inserted(0));

    let out = one(&mut g, "select ?n by ?n where ?p a person ; name ?n");
    assert_eq!(data(&g, rows(&out)), [["Alice"], ["Bob"], ["Carol"]]);
    let Output::Rows { vars, .. } = one(&mut g, "select * where ?p name ?n ; age ?a") else {
        panic!()
    };
    assert_eq!(vars, ["p", "n", "a"]);

    // Comparisons of data, with literals or variables, and of ids.
    let out = one(
        &mut g,
        "select ?n by ?n where ?p name ?n ; age ?a . ?a >= 30",
    );
    assert_eq!(data(&g, rows(&out)), [["Alice"], ["Carol"]]);
    let out = one(&mut g, "select ?n where ?p name ?n ; age ?a . 30 > ?a");
    assert_eq!(data(&g, rows(&out)), [["Bob"]]);
    let out = one(
        &mut g,
        "select ?n, ?m where ?p name ?n ; age ?a ; knows ?q . ?q name ?m ; age ?b . ?a < ?b",
    );
    let mut pairs = data(&g, rows(&out));
    pairs.sort();
    assert_eq!(pairs, [["Alice", "Carol"], ["Bob", "Carol"]]);
    let out = one(
        &mut g,
        "select ?n where /alice knows ?p . ?p name ?n . ?p != /bob",
    );
    assert_eq!(data(&g, rows(&out)), [["Carol"]]);

    // Or, not, negated subjects and predicates.
    let out = one(
        &mut g,
        "select distinct ?p by ?n where ?p name ?n and (?p knows /carol or /alice knows ?p)",
    );
    assert_eq!(rows(&out).len(), 3);
    let out = one(&mut g, "select ?n where ?p name ?n . not ?p banned true");
    assert_eq!(data(&g, rows(&out)).len(), 2);
    let out = one(&mut g, "select ?n where ?p name ?n ; not banned true");
    assert_eq!(data(&g, rows(&out)).len(), 2);
    let out = one(&mut g, "select ?n where ?p name ?n and not (?p knows ?)");
    assert_eq!(data(&g, rows(&out)), [["Carol"]]);
    let out = one(&mut g, "select ?n limit 2 where ?p name ?n");
    assert_eq!(rows(&out).len(), 2);

    // Unknown constants match nothing.
    assert!(rows(&one(&mut g, "select ?x where ?x knows /nobody")).is_empty());

    assert_eq!(
        one(&mut g, "verify /alice knows /carol"),
        Output::Verified(true)
    );
    assert_eq!(
        one(&mut g, "verify /carol knows ?x"),
        Output::Verified(false)
    );

    // The links matched.
    let Output::Graph(graph) = one(&mut g, "lookup /alice knows ?x . ?x age ?a") else {
        panic!()
    };
    let links: Vec<u64> = graph.stored().collect();
    assert_eq!(links.len(), 4);
    for l in &links {
        assert!(g.get(*l).unwrap().is_link());
    }

    // Insert and delete for each match.
    let out = one(
        &mut g,
        "insert ?p friend ?q . ?p met [ at ?q ] where ?p knows ?q . ?q knows ?",
    );
    assert_eq!(out, Output::Inserted(3));
    let out = one(&mut g, "select ?p where ?p met ?m . ?m at /carol");
    assert_eq!(rows(&out).len(), 0);
    let out = one(&mut g, "select ?p where ?p met ?m . ?m at /bob");
    assert_eq!(rows(&out).len(), 1);
    assert_eq!(
        one(
            &mut g,
            "delete ?p knows ?q where ?p knows ?q . ?q banned true"
        ),
        Output::Deleted(2)
    );
    assert_eq!(
        one(&mut g, "verify ?p knows /carol"),
        Output::Verified(false)
    );
    assert_eq!(one(&mut g, "delete /alice knows /bob"), Output::Deleted(1));
    assert_eq!(one(&mut g, "delete /alice knows /bob"), Output::Deleted(0));
}

#[test]
fn same_as_queries() {
    // A GraQL query and the same query built by hand give the same rows.
    let mut g = graph();
    execute(
        &mut g,
        "insert /a p /b . /b p /c . /c p /a . /a q /c . /b q /a",
    )
    .unwrap();
    let (q, vars) = parse("select ?x, ?z where ?x p ?y . ?y p ?z . not ?x q ?z").unwrap()[0]
        .query(&g)
        .unwrap();
    assert_eq!(vars, ["x", "z"]);
    let [a, b, c] = ["/a", "/b", "/c"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let (p, qq) = (g.find_value(b"p").unwrap(), g.find_value(b"q").unwrap());
    let mut hand = Query::new();
    let (x, y, z) = (hand.var("x"), hand.var("y"), hand.var("z"));
    let mut not = Group::new();
    not.pattern(x, qq, z);
    hand.pattern(x, p, y)
        .pattern(y, p, z)
        .not(not)
        .select([x, z]);
    let mut got = q.run(&g).unwrap();
    let mut expect = hand.run(&g).unwrap();
    got.sort();
    expect.sort();
    assert_eq!(got, expect);
    assert_eq!(got, [[c, b]]);
    let _ = (a, b);
}

#[test]
fn unsupported() {
    let unsupported = |text: &str| match parse(text) {
        Err(Error::Unsupported { what, .. }) => what,
        other => panic!("{text}: {other:?}"),
    };
    assert_eq!(
        unsupported("select ?x where ?x knows ?y * ; p ?z"),
        "predicate modifiers"
    );
    assert_eq!(
        unsupported("select ?x where ?x knows++ ?y ?author"),
        "author and subgraph positions of a path"
    );
    assert!(matches!(
        parse("select ?x where let ?x in ?x p ?y"),
        Err(Error::Syntax { .. })
    ));
    assert_eq!(
        unsupported("select ?x where ?x p ?y implies ?x q ?y"),
        "'implies'"
    );
    assert_eq!(unsupported("select ?x where f(?x) @ /g"), "'@'");
    // `(` after a subject starts a path expression, whose predicates must
    // be constants.
    assert!(matches!(
        parse("select ?x where /a(?x) ?y"),
        Err(Error::Syntax { .. })
    ));
    assert_eq!(unsupported("select ?x where ?x p list(?a)"), "lists");
    // A function's parameters are bare names.
    assert!(matches!(
        parse("function f(?x) ?x"),
        Err(Error::Syntax { .. })
    ));
    assert_eq!(
        unsupported("select ?x from ( lookup ?x p ?y ) where ?x p ?y"),
        "a graph other than a subgraph's name"
    );
    // `as` names a column with a variable.
    assert!(matches!(
        parse("select ?x as y where ?x p ?y"),
        Err(Error::Syntax { .. })
    ));
    assert!(matches!(
        parse("select ?x where\n  ?x p ?y ,"),
        Err(Error::Syntax {
            line: 2,
            column: 12,
            ..
        })
    ));
    assert!(matches!(parse("select ?x"), Err(Error::Syntax { .. })));

    // Errors when run.
    let mut g = graph();
    assert!(matches!(
        execute(&mut g, "select ?y where ?x p ?z"),
        Err(Error::Syntax { .. })
    ));
    assert!(matches!(
        execute(&mut g, "select ?x where ?x p ?y or ?x q ?z"),
        Err(Error::Graph(_))
    ));
    assert!(execute(&mut g, "insert ?x p ?y where ?x q ?z").is_err());
}

#[test]
fn paths_and_rules() {
    let mut g = graph();
    let mut session = Session::new();
    session
        .execute(
            &mut g,
            "insert /a parent /b . /b parent /c . /c parent /d . /a age 70 . /b age 40 . /c age 20 . /d age 1",
        )
        .unwrap();
    let sorted_rows = |out: &Output| -> Vec<Vec<u64>> {
        let mut rows = rows(out).to_vec();
        rows.sort();
        rows
    };
    let [a, b, c, d] = ["/a", "/b", "/c", "/d"].map(|k| g.find_by_key(k.as_bytes()).unwrap());

    // Transitive predicates.
    let out = one(&mut g, "select ?x where /a parent++ ?x");
    assert_eq!(sorted_rows(&out), [[b], [c], [d]]);
    let out = one(&mut g, "select ?x where ?x parent++ /d ; age ?n . ?n < 50");
    assert_eq!(sorted_rows(&out), [[b], [c]]);
    assert_eq!(
        one(&mut g, "verify /d parent++ /a"),
        Output::Verified(false)
    );

    // Rules, in a session: defined, then called, recursively.
    let out = session
        .execute(
            &mut g,
            "rule ancestor(?x ?y) ?x parent ?y
             rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)
             select ?y where ancestor(/b ?y)",
        )
        .unwrap();
    assert_eq!(out[0], Output::Defined("ancestor".into()));
    assert_eq!(sorted_rows(&out[2]), [[c], [d]]);
    // Later statements see them, and so do other sessions: they are saved.
    let out = session
        .execute(
            &mut g,
            "select ?x, ?y where ancestor(?x ?y) . not ?x parent ?y",
        )
        .unwrap();
    assert_eq!(sorted_rows(&out[0]), [[a, c], [a, d], [b, d]]);
    let out = execute(&mut g, "select ?y where ancestor(/b ?y)").unwrap();
    assert_eq!(sorted_rows(&out[0]), [[c], [d]]);
    // Rules using negation of other rules; calls in templates fail.
    let out = session
        .execute(
            &mut g,
            "rule root(?x) ?x parent ? . not ancestor(? ?x)
             select ?x where root(?x)",
        )
        .unwrap();
    assert_eq!(sorted_rows(&out[1]), [[a]]);
    assert!(session.execute(&mut g, "insert ancestor(/d /a)").is_err());
    // A rule depending on its own negation is not stratified.
    let err = session.execute(
        &mut g,
        "rule odd(?x) ?x parent ? . not odd(?x) select ?x where odd(?x)",
    );
    assert!(matches!(err, Err(Error::Graph(_))), "{err:?}");
}

#[test]
fn incomplete() {
    for text in [
        "select ?x",
        "select ?x where",
        "select ?x where /a knows ?x , ",
        "select ?x where /a knows ?x and",
        "insert /a name \"unfinished",
        "insert /a /* comment",
        "select ?x where ( /a knows ?x",
        "insert /a knows /b . /b",
    ] {
        assert!(is_incomplete(text), "{text}");
    }
    for text in [
        "",
        "select ?x where /a knows ?x",
        "select ?x where /a knows ?x .",
        "select ?x where /a knows ?x ;",
        "insert /a knows /b",
        "select ?x where /a knows ?x insert /a b /c",
        "select ?x where ) /a",
        "select ?x where /a knows ?x ]",
        "quit",
        "insert /a name \"x\" # junk",
    ] {
        assert!(!is_incomplete(text), "{text}");
    }
}

#[test]
fn session_rules() {
    let mut g = graph();
    let mut s = Session::new();
    s.execute(&mut g, "rule r(?x ?y) ?x knows ?y").unwrap();
    let rules = s.rules(&g).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "r");
    assert_eq!(rules[0].head, ["x", "y"]);
    assert!(keywords().contains(&"select"));
}

/// `p` with nested `and`s and `or`s flattened, and `and`s and `or`s of one
/// pattern replaced by it.
fn flat(p: &Pattern) -> Pattern {
    let group = |ps: &[Pattern], or: bool| {
        let mut out = Vec::new();
        for c in ps {
            match (flat(c), or) {
                (Pattern::And(cs), false) | (Pattern::Or(cs), true) => out.extend(cs),
                (c, _) => out.push(c),
            }
        }
        match (out.len(), or) {
            (1, _) => out.pop().unwrap(),
            (_, false) => Pattern::And(out),
            (_, true) => Pattern::Or(out),
        }
    };
    match p {
        Pattern::And(ps) => group(ps, false),
        Pattern::Or(ps) => group(ps, true),
        Pattern::Not(q) => Pattern::Not(Box::new(flat(q))),
        Pattern::Window(q, id) => Pattern::Window(Box::new(flat(q)), *id),
        Pattern::Optional(q) => Pattern::Optional(Box::new(flat(q))),
        p => p.clone(),
    }
}

/// Renames the blank nodes of `p` `0`, `1`, ... in order of appearance.
fn rename_blanks(p: &mut Pattern, names: &mut HashMap<String, String>) {
    let mut node = |n: &mut Node| {
        if let Node::Blank(b) = n {
            let len = names.len();
            *b = names
                .entry(b.clone())
                .or_insert_with(|| len.to_string())
                .clone();
        }
    };
    match p {
        Pattern::Node(n) => node(n),
        Pattern::Tagged(s, pr, o, author, graph) => {
            node(s);
            node(pr);
            node(o);
            author.iter_mut().for_each(&mut node);
            graph.iter_mut().for_each(&mut node);
        }
        Pattern::InGraph(p, g) => {
            node(g);
            rename_blanks(p, names);
        }
        Pattern::Triple(s, pr, o) | Pattern::Path(s, pr, o) => {
            node(s);
            node(pr);
            node(o);
        }
        Pattern::PathExpr(s, _, o) => {
            node(s);
            node(o);
        }
        Pattern::Link(l, s, pr, o) => {
            node(l);
            node(s);
            node(pr);
            node(o);
        }
        Pattern::Call(_, args) => args.iter_mut().for_each(node),
        Pattern::Compare(a, _, b) => {
            a.each_node_mut(&mut node);
            b.each_node_mut(&mut node);
        }
        Pattern::Test(e) | Pattern::Let(_, e) => e.each_node_mut(&mut node),
        Pattern::LimitValues { .. } => {}
        Pattern::LinkAggregate { triple, .. } => {
            let (s, pr, o) = &mut **triple;
            node(s);
            node(pr);
            node(o);
        }
        Pattern::And(ps) | Pattern::Or(ps) | Pattern::Assert(ps, _) => {
            ps.iter_mut().for_each(|p| rename_blanks(p, names))
        }
        Pattern::Not(p) | Pattern::Window(p, _) | Pattern::Optional(p) => rename_blanks(p, names),
    }
}

/// `s`, compared up to how its patterns are grouped and its blanks named.
fn normal(s: &Statement) -> Statement {
    let mut names = HashMap::new();
    let pattern = |p: &Pattern, names: &mut HashMap<String, String>| {
        let mut p = flat(p);
        rename_blanks(&mut p, names);
        p
    };
    match s {
        Statement::Rule(r) => Statement::Rule(RuleDef {
            name: r.name.clone(),
            head: r.head.clone(),
            body: pattern(&r.body, &mut names),
        }),
        Statement::Select {
            distinct,
            vars,
            having,
            by,
            pattern: p,
            limit,
            offset,
            as_of,
        } => Statement::Select {
            distinct: *distinct,
            vars: vars.clone(),
            having: having.clone(),
            by: by.clone(),
            pattern: pattern(p, &mut names),
            limit: *limit,
            offset: *offset,
            as_of: *as_of,
        },
        Statement::Verify { pattern: p, as_of } => Statement::Verify {
            pattern: pattern(p, &mut names),
            as_of: *as_of,
        },
        Statement::Lookup {
            pattern: p,
            where_,
            as_of,
        } => Statement::Lookup {
            pattern: pattern(p, &mut names),
            where_: where_.as_ref().map(|w| pattern(w, &mut names)),
            as_of: *as_of,
        },
        Statement::Insert {
            template,
            pattern: p,
            graph,
        }
        | Statement::Delete {
            template,
            pattern: p,
            graph,
        } => {
            let mut t = Pattern::And(template.clone());
            rename_blanks(&mut t, &mut names);
            let Pattern::And(template) = t else {
                unreachable!()
            };
            let p = p.as_ref().map(|p| pattern(p, &mut names));
            if matches!(s, Statement::Insert { .. }) {
                Statement::Insert {
                    template,
                    pattern: p,
                    graph: graph.clone(),
                }
            } else {
                Statement::Delete {
                    template,
                    pattern: p,
                    graph: graph.clone(),
                }
            }
        }
        Statement::DeleteRule(n) => Statement::DeleteRule(n.clone()),
        s @ (Statement::IndexVector { .. } | Statement::DeleteIndex(_)) => s.clone(),
        Statement::InsertGraph { source, graph } => Statement::InsertGraph {
            source: normal_graph(source),
            graph: graph.clone(),
        },
        Statement::From { source, statement } => Statement::From {
            source: normal_graph(source),
            statement: Box::new(normal(statement)),
        },
        Statement::Set { name, value } => Statement::Set {
            name: name.clone(),
            value: match value {
                SetValue::Graph(e) => SetValue::Graph(normal_graph(e)),
                v => v.clone(),
            },
        },
        Statement::Function { .. } => s.clone(),
        Statement::Test {
            description,
            operands,
        } => Statement::Test {
            description: description.clone(),
            operands: Box::new(operands.clone().map(|o| match o {
                TestOperand::Statement(s) => TestOperand::Statement(Box::new(normal(&s))),
                TestOperand::Graph(e) => TestOperand::Graph(normal_graph(&e)),
            })),
        },
        Statement::Data(e) => Statement::Data(normal_graph(e)),
        Statement::Default(e) => Statement::Default(normal_graph(e)),
        Statement::Assume { template, graph } => {
            let mut t = Pattern::And(template.clone());
            rename_blanks(&mut t, &mut names);
            let Pattern::And(template) = t else {
                unreachable!()
            };
            Statement::Assume {
                template,
                graph: graph.clone(),
            }
        }
        Statement::Author(_) | Statement::Subgraph(_) => s.clone(),
        Statement::Maintain {
            template,
            pattern: p,
            graph,
        } => {
            let mut t = Pattern::And(template.clone());
            rename_blanks(&mut t, &mut names);
            let Pattern::And(template) = t else {
                unreachable!()
            };
            Statement::Maintain {
                template,
                pattern: pattern(p, &mut names),
                graph: graph.clone(),
            }
        }
    }
}

/// A graph, compared up to how its lookups are grouped and its blanks
/// named.
fn normal_graph(e: &GraphExpr) -> GraphExpr {
    match e {
        GraphExpr::Name(_) => e.clone(),
        GraphExpr::Literal(ps) => {
            let mut t = Pattern::And(ps.clone());
            rename_blanks(&mut t, &mut HashMap::new());
            let Pattern::And(ps) = t else { unreachable!() };
            GraphExpr::Literal(ps)
        }
        GraphExpr::Over(b, e) => GraphExpr::Over(b.clone(), Box::new(normal_graph(e))),
        GraphExpr::Lookup(s) => GraphExpr::Lookup(Box::new(normal(s))),
    }
}

#[test]
fn print_round_trips() {
    for text in [
        "select ?who, ?age by ?age where /alice knows ?who . ?who age ?age . ?age < 30",
        "select distinct * limit 5 where ?x a person ; name ?n , ?m",
        "select ?x where ?x a type",
        "verify not /a knows /b",
        "verify /a not knows /b",
        "verify not ( /a knows /b or /b knows /a )",
        "lookup ?x knows [ age 30 ; name \"Bo b\" ]",
        "lookup ?x knows [ :?y age 30 ]",
        "select ?x where ( ?x a cat or ?x a dog ) and not ( ?x age ?a . ?a > 10 )",
        "select ?x where ?x a cat or ?x a dog and ?x age 3 or ?x a fox",
        "select ?x where ?x p ? . ? q ?x",
        "select ?x where _:b1 p ?x . ? q _:b1 . _:b2 r ?",
        "select ?x where { ?x p 1 . ?x q 2 }",
        "select ?x where not /a knows ?x , ?y",
        "select ?x where /a p++ ?x . ?x != /b . ?x = ?y . ?y >= 2 . ?y <= 9",
        "select ?x where ?x p \"select\" ; q \"a\" ; r \"3.\" ; s 3.5 ; t \"say \\\"hi\\\"\\n\" ; u true ; v \"\"",
        "select ?x where ?x p \"-3\" ; q \"/not/a/key\" ; r \"tab\\there\" ; s \"Ünï\"",
        "rule reach(?a ?b) ?a knows ?m . reach(?m ?b)",
        "rule root(?x) ?x parent ? . not ancestor(? ?x)",
        "rule r(?x ?y) ( ?x p ?y or ?y p ?x ) and ?x q 1",
        "insert /a knows _:n . _:n name \"x y\" where /a age 3",
        "insert [ a person ] name bob",
        "insert /a knows /b , /c ; age 30",
        "delete /a knows ?x where /a knows ?x",
        "delete /a knows /b",
        "delete rule reach",
        "index vector on embedding dims 3 metric l2 m 8 ef 100",
        "index vector on \"two words\" dims 768 metric cosine m 16 ef 200",
        "delete index vector on embedding",
        "select ?v where nearest(embedding $q 10 ?v ?d dot)",
        "select ?who, ?y where << /alice knows ?who >> since ?y",
        "select ?l where << :?l /alice knows ?x >>",
        "select ?l where << :_:k /a knows ?x >> . _:k since ?l",
        "select ?s where << << /a knows /b >> since 2019 >> source ?s",
        "select ?o where ?n about << /a knows /b >> . ?o p << ?n a note >>",
        "verify not << /a knows /b >> . /a p 1",
        "select ?x limit 3 as of 120 where /a knows ?x",
        "verify as of 7 /a knows /b",
        "lookup as of 7 /a knows ?x",
        "select ?x where ( /a knows ?x ) as of 120 and not ( /a knows ?x )",
        "select ?x where ( ?x p 1 or ( ?x q 2 ) as of 5 ) as of 9",
        "insert << /a knows /b >> since 2019 ; source /census",
        "insert << :?l /a knows /b >> . ?l since 2019",
        "delete ?l since 2019 where << :?l /a knows /b >>",
        "select ?x as of \"2026-10-31T12:30:00Z\" where /a knows ?x",
        "verify as of \"2026-10-31T14:30:00.250+02:00\" /a knows /b",
        "select ?x where ( /a knows ?x ) as of \"2026-01-01\" and not ( /a knows ?x )",
        "select ?p, ?e where ?p a person and optional ( ?p email ?e )",
        "select ?p, ?e where ?p a person . optional ( ?p email ?e . ?e verified ? )",
        "select ?p where ?p a person ; email ?e - ; name ?n",
        "select ?p where ?p a person and optional ( ?p email ?e and optional ( ?e ok ?k ) )",
        "select ?p where ( ?p a cat or ?p a dog ) and optional ( ?p name ?n ) and not ( ?n = x )",
        "insert ?p contact ?e where ?p a person and optional ( ?p email ?e )",
        "select ?c, count(?p) as ?n by ?n desc, ?c where ?p city ?c",
        "select count(*) where ?p a person",
        "select count(distinct ?c) as ?k, min(?a), max(?a) as ?oldest where ?p city ?c ; age ?a",
        "select ?c, avg(?a) as ?mean, sum(?a) having ?mean > 30 and ?c != rome where ?p city ?c ; age ?a",
        "select distinct ?c, sample(?p) limit 2 where ?p city ?c",
        "select ?x by ?x desc where ?x age ?a",
        "select ?y where ?x (knows / likes) ?y",
        "select ?y where ?x (knows/likes) ?y",
        "select ?y where /a (knows/likes/hates+) ?y",
        "select ?y where ?x (^parent | guardian) ?y",
        "select ?y where ?x (knows*) ?y . ?x (knows{1,3}) ?y . ?x (p{2}) ?y . ?x (p{2,}) ?y",
        "select ?y where ?x ((knows / likes)+ | ^\"two words\") ?y",
        "select ?y where ?x (a / ^a) ?y ; (p | q)++ ?z , ?w",
        "select ?y where ?x not (p | q) ?y ; (r+) ?z - . ?z p 1",
        "select ?f where $who knows ?f ; age ?a . ?a > $min",
        "insert $who knows #12 where #12 a person",
        "select ?f by ?f limit 3 offset 6 where ?x knows ?f",
        "select ?c, count(?p) as ?n having ?n > $least by ?n desc offset 1 where ?p city ?c",
        "select ?p, ?price * ?qty as ?total where ?p price ?price ; qty ?qty . ?price * ?qty > 100",
        "select upper(?n), length(?n) - 1 where ?p name ?n . contains(?n, \"a\") . not starts(?n, b)",
        "select ?t where ?p price ?x . let ?t = (?x + 2) * -?x / 3 % 7 . ?t >= 0",
        "select ?a - (?b - ?c), ?a - ?b - ?c, ?a * (?b + ?c) where ?p a ?a ; b ?b ; c ?c",
        "select ?c, sum(?p * ?q) as ?revenue having ?revenue > 10 * $k where ?o city ?c ; p ?p ; q ?q",
        "select ?x as ?y, concat(str(?x), \" \", iso(when(?l))) where << :?l ?x knows ? >>",
        "select ?p where ?p age ?a and optional ( ?p email ?e ) and not ( bound(?e) = true )",
        "insert ?x knows ?y into friends where ?x knows ?y ; city paris",
        "delete /a knows /b from friends",
        "select ?x from friends where ?x knows ?y",
        "lookup ?x knows ?y in /g/friends",
        "verify /a knows /b in friends",
        "select ?x where ( ?x knows /a ) in old and ?x city oslo",
        "select ?g where /a knows /b ? ?g",
        "select ?s where ?s likes ?o /me . ?s knows ?o /me #12",
        "insert /a knows /b /me friends . /b knows /c",
        "insert graph ( lookup ?x knows ?y in old ) into archive",
        "insert graph ( lookup ?x knows ?y )",
        "author /author/john",
        "subgraph $g",
        "maintain ?x knows ?y . ?y knows ?x into mutual where ?x knows ?y . ?y knows ?x",
        "lookup ?p name ?n . ?p alive true!",
        "lookup ?p name ?n ; pet ?x - . ?p pet_lover true! @?x",
        "lookup ?p pet ?x . ?p record [ animal ?x ; kind [ name pet ] ]! @?x",
        "lookup ?p pet ?x . ?p tag x! , y! ; seen ?x",
        "rule current(?p ?o) ?m spouse ?p , ?o . ?p current_spouse ?o!",
        "lookup ?g parent ?p . ?p parent ?c . COUNT(?g grandkids ?c)",
        "lookup ?g parent ?p . ?p parent ?c . COUNT(distinct ?p kids ?c) @?g",
        "select ?m where ?m spouse ?p ; began ?f . ORDER(?m rank ?f desc) @?p . ?m rank (= 0)",
        "lookup ?m began ?f . REVERSE_ORDER(?m r ?f) . DENSE_RANK(?m d ?f) . ROW_NUMBER(?m n ?f desc)",
        "rule oldest(?p) ?p age ?a . RANK(?p r ?a desc) . ?p r 0",
        "set me = /joe",
        "set heavy = 100",
        "set greeting = \"Joe\"",
        "set g = graph { /a knows /b . not /c knows /d }",
        "set h = graph g:{ /x knows ?y . ?y name z }",
        "set k = graph g",
        "set l = graph ( lookup ?x knows ?y )",
        "set e = graph {}",
        "data {}",
        "data database",
        "default { /p gloss \"the p\" ; cardinality 1 }",
        "assume /woody born 1935 in facts",
        "assume /a knows /b",
        "select ?x from graph g where ?x knows ?y",
        "select ?x, count(?y) by ?x from graph database:{ /a knows /b } where ?x knows ?y",
        "lookup ?x knows ?y in graph more where ?x a person",
        "verify /a knows /b in graph g:{ /a knows /b }",
        "insert graph g into archive",
        "insert graph ( lookup ?x knows ?y in graph g )",
        "lookup ?p a person limit 2",
        "lookup ?people limit 10 and ?p type ?t limit 3",
        "select ?p, ?pet where ?p a person and ?p pet ?pet ?pet limit 1 offset 2",
        "test \"same\" lookup ?d where /a d ?d lookup ?d where /b d ?d",
        "test select ?x where ?x a b graph { /x a b }",
        "function kg(lbs) lbs / 2.2",
        "function size(m, n) if m > 50 || n = 0 && m < 3 then \"big\" else concat(str(m), \"!\")",
        "select kg(?m) as ?kg where ?p mass ?m . kg(?m) > 3",
        "insert simple_property(/film/film/directed_by \"the director\")",
        "set g = graph { property(/a 0 1) . /x y z }",
    ] {
        let parsed = parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(parsed.len(), 1, "{text}");
        let printed = parsed[0].to_string();
        let reparsed = parse(&printed).unwrap_or_else(|e| panic!("{text}\n -> {printed}: {e}"));
        assert_eq!(reparsed.len(), 1, "{printed}");
        assert_eq!(
            normal(&reparsed[0]),
            normal(&parsed[0]),
            "{text}\n -> {printed}"
        );
        // The printed text is canonical.
        assert_eq!(reparsed[0].to_string(), printed, "{text}");
    }
}

#[test]
fn literals() {
    for (data, text) in [
        ("bob", "bob"),
        ("30", "30"),
        ("3.5", "3.5"),
        ("3.", "\"3.\""),
        ("-3", "\"-3\""),
        ("a", "\"a\""),
        ("select", "\"select\""),
        ("Select", "\"Select\""),
        ("two words", "\"two words\""),
        ("/alice", "\"/alice\""),
        ("say \"hi\"\n", "\"say \\\"hi\\\"\\n\""),
        ("", "\"\""),
    ] {
        assert_eq!(literal(data), text);
    }
}

/// The rule links saved in `g`.
fn saved_links(g: &Graph) -> Vec<u64> {
    Session::new()
        .saved_rules(g)
        .unwrap()
        .iter()
        .map(|&(l, _)| l)
        .collect()
}

#[test]
fn rules_are_saved() {
    let dir = std::env::temp_dir().join(format!("graqlite-graql-{}-saved", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prefix = dir.join("g.");
    {
        let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
        execute(
            &mut g,
            "insert /a parent /b . /b parent /c
             rule ancestor(?x ?y) ?x parent ?y
             rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)",
        )
        .unwrap();
        g.commit().unwrap();
    }
    let mut g = Graph::open(Some(&prefix), Options::default()).unwrap();
    let c = g.find_by_key(b"/c").unwrap();
    let out = one(&mut g, "select ?y where ancestor(/a ?y) . ?y = /c");
    assert_eq!(rows(&out), [[c]]);
    let rules = Session::new().rules(&g).unwrap().to_vec();
    assert_eq!(
        rules.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
        [
            "rule ancestor(?x ?y) ?x parent ?y",
            "rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)"
        ]
    );
    drop(g);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn saving_a_rule_again_does_nothing() {
    let mut g = graph();
    one(&mut g, "rule r(?x ?y) ?x knows ?y");
    let next = g.next_id();
    // The same rule, written differently.
    let out = one(&mut g, "rule r(?x, ?y)   { ?x knows ?y }");
    assert_eq!(out, Output::Defined("r".into()));
    assert_eq!(g.next_id(), next);
    assert_eq!(saved_links(&g).len(), 1);
}

#[test]
fn saved_rules_roll_back() {
    let mut g = graph();
    let mut s = Session::new();
    s.execute(&mut g, "rule kept(?x) ?x p 1").unwrap();
    g.commit().unwrap();
    s.execute(&mut g, "rule lost(?x) ?x p 2").unwrap();
    assert_eq!(s.rules(&g).unwrap().len(), 2);
    g.rollback().unwrap();
    let rules = s.rules(&g).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "kept");
}

#[test]
fn deleting_rules() {
    let mut g = graph();
    execute(
        &mut g,
        "insert /a parent /b
         rule ancestor(?x ?y) ?x parent ?y
         rule ancestor(?x ?y) ?x parent ?z . ancestor(?z ?y)
         rule other(?x) ?x parent /b",
    )
    .unwrap();
    assert_eq!(rules_using(&g, b"parent").unwrap().len(), 3);
    assert_eq!(
        one(&mut g, "delete rule ancestor"),
        Output::DeletedRule {
            name: "ancestor".into(),
            definitions: 2
        }
    );
    // Its reference links went with it.
    assert_eq!(rules_using(&g, b"parent").unwrap(), saved_links(&g));
    assert!(rules_using(&g, b"ancestor").unwrap().is_empty());
    let err = execute(&mut g, "select ?y where ancestor(/a ?y)").unwrap_err();
    assert!(err.to_string().contains("unknown rule"), "{err}");
    let err = execute(&mut g, "delete rule ancestor").unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
    assert_eq!(err.to_string(), "there is no rule 'ancestor'");
}

#[test]
fn rule_references() {
    let mut g = graph();
    one(&mut g, "insert /alice knows /bob");
    one(
        &mut g,
        "rule friend(?x) /alice knows ?x . reach(?x /carol) . ?x age ?a . ?a > 18",
    );
    let [link] = saved_links(&g)[..] else {
        panic!()
    };
    for name in ["knows", "age", "18", "/alice", "/carol", "reach"] {
        assert_eq!(rules_using(&g, name.as_bytes()).unwrap(), [link], "{name}");
    }
    assert!(rules_using(&g, b"/bob").unwrap().is_empty());
    // A key reference stands for the key, not the entity with it, which
    // need not exist.
    let alice = g.find_by_key(b"/alice").unwrap();
    let reference = key_reference(&g, b"/alice").unwrap();
    assert_ne!(reference, alice);
    assert!(g.find_by_key(b"/carol").is_none());
    assert!(key_reference(&g, b"/carol").is_some());
    // References are shared.
    one(&mut g, "rule enemy(?x) /alice hates ?x");
    assert_eq!(key_reference(&g, b"/alice"), Some(reference));
    assert_eq!(rules_using(&g, b"/alice").unwrap().len(), 2);
}

#[test]
fn rebuilding_references() {
    let mut g = graph();
    one(&mut g, "rule r(?x) ?x knows /bob");
    let [link] = saved_links(&g)[..] else {
        panic!()
    };
    assert_eq!(rebuild_references(&mut g).unwrap(), 0);
    let uses = g.find_value(USES_PREDICATE).unwrap();
    let knows = g.find_value(b"knows").unwrap();
    g.delete_link(link, uses, knows).unwrap().unwrap();
    assert!(rules_using(&g, b"knows").unwrap().is_empty());
    assert_eq!(rebuild_references(&mut g).unwrap(), 1);
    assert_eq!(rules_using(&g, b"knows").unwrap(), [link]);
    assert_eq!(rebuild_references(&mut g).unwrap(), 0);
}

#[test]
fn unparsable_saved_rules_are_skipped() {
    let mut g = graph();
    one(&mut g, "insert /a p 1");
    one(&mut g, "rule good(?x) ?x p 1");
    // Text saved by something else, which does not parse.
    let node = g.node_by_key(RULES_KEY).unwrap();
    let predicate = g.value(RULE_PREDICATE).unwrap();
    let text = g.value(b"rule broken(?x) ?x p <<< 1").unwrap();
    let link = g.link(node, predicate, text).unwrap();
    let mut s = Session::new();
    let skipped = s.skipped_rules(&g).unwrap().to_vec();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].link, link);
    assert!(!skipped[0].error.is_empty());
    let a = g.find_by_key(b"/a").unwrap();
    let out = s.execute(&mut g, "select ?x where good(?x)").unwrap();
    assert_eq!(rows(&out[0]), [[a]]);
    // It can still be deleted by name.
    let out = s.execute(&mut g, "delete rule broken").unwrap();
    assert_eq!(
        out[0],
        Output::DeletedRule {
            name: "broken".into(),
            definitions: 1
        }
    );
    assert!(s.skipped_rules(&g).unwrap().is_empty());
}

#[test]
fn a_bad_rule_affects_only_its_callers() {
    let mut g = graph();
    one(&mut g, "insert /a parent /b");
    // It calls a rule never defined: it is saved, but cannot be evaluated.
    one(&mut g, "rule orphan(?x) ?x parent ? . missing(?x)");
    one(&mut g, "rule parent_of(?x ?y) ?x parent ?y");
    let out = one(&mut g, "select ?y where parent_of(/a ?y)");
    assert_eq!(rows(&out).len(), 1);
    let err = execute(&mut g, "select ?x where orphan(?x)").unwrap_err();
    assert!(err.to_string().contains("unknown rule 'missing'"), "{err}");
}

#[test]
fn rules_may_call_rules_defined_later() {
    let mut g = graph();
    one(&mut g, "insert /a p 1");
    one(&mut g, "rule first(?x) second(?x)");
    one(&mut g, "rule second(?x) ?x p 1");
    let a = g.find_by_key(b"/a").unwrap();
    assert_eq!(rows(&one(&mut g, "select ?x where first(?x)")), [[a]]);
}

#[test]
fn invalid_rules_are_not_saved() {
    let mut g = graph();
    for text in [
        "rule r(?x) ?x p 1 or ?y q 2",
        "rule r(?x ?y) ?x p 1",
        "rule odd(?x) ?x parent ? . not odd(?x)",
    ] {
        let err = execute(&mut g, text).unwrap_err();
        assert!(matches!(err, Error::Graph(_)), "{text}: {err:?}");
        assert!(saved_links(&g).is_empty(), "{text}");
    }
    // Not stratified once the rule it calls is defined.
    one(&mut g, "rule odd(?x) ?x parent ? . not even(?x)");
    let err = execute(&mut g, "rule even(?x) ?x parent ? . odd(?x)").unwrap_err();
    assert!(err.to_string().contains("stratified"), "{err}");
    assert_eq!(saved_links(&g).len(), 1);
}

#[test]
fn rule_variables_are_not_the_query_s() {
    let mut g = graph();
    one(&mut g, "insert /a p 1");
    one(&mut g, "rule r(?y) ?y p 1");
    let err = execute(&mut g, "select ?y where r(?x)").unwrap_err();
    assert!(
        err.to_string().contains("?y is not in the pattern"),
        "{err}"
    );
}

#[test]
fn statement_lines() {
    let lines: Vec<usize> = parse_with_lines(
        "insert /a p 1\n\n-- a comment\nselect ?x\n  where ?x p 1 verify /a p 1\n\n  rule r(?x) ?x p 1",
    )
    .unwrap()
    .into_iter()
    .map(|(l, _)| l)
    .collect();
    assert_eq!(lines, [1, 4, 5, 7]);
}

#[test]
fn link_nodes() {
    let mut g = graph();
    one(&mut g, "insert /alice knows /bob , /carol");
    let [bob, carol] = ["/bob", "/carol"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let alice = g.find_by_key(b"/alice").unwrap();
    let knows = g.find_value(b"knows").unwrap();
    let to_bob = g.find_link(alice, knows, bob).unwrap();
    // A link node binds the link.
    let out = one(
        &mut g,
        "select ?l, ?who where << :?l /alice knows ?who >> . ?who = /bob",
    );
    assert_eq!(rows(&out), [[to_bob, bob]]);
    // Statements about a link, inserted and queried; the link itself is
    // there already, so only the statements are new.
    assert_eq!(
        one(
            &mut g,
            "insert << /alice knows /bob >> since 2019 ; source /census"
        ),
        Output::Inserted(2)
    );
    let since = g.find_value(b"since").unwrap();
    assert!(
        g.find_link(to_bob, since, g.find_value(b"2019").unwrap())
            .is_some()
    );
    let out = one(
        &mut g,
        "select ?who, ?y where << /alice knows ?who >> since ?y",
    );
    assert_eq!(rows(&out), [[bob, g.find_value(b"2019").unwrap()]]);
    // Nested: a statement about a statement about a link.
    one(
        &mut g,
        "insert << << /alice knows /carol >> since 2020 >> source /letters",
    );
    let out = one(
        &mut g,
        "select ?who where << << /alice knows ?who >> since ? >> source /letters",
    );
    assert_eq!(rows(&out), [[carol]]);
    // Inserting a link node's triple links it, if it is not there.
    assert_eq!(
        one(&mut g, "insert << /alice knows /dave >> since 2024"),
        Output::Inserted(2)
    );
    assert_eq!(
        one(&mut g, "verify /alice knows /dave"),
        Output::Verified(true)
    );
    // A named link node in a template is bound by it.
    one(&mut g, "insert << :?l /eve knows /fay >> . ?l since 2001");
    let out = one(&mut g, "verify << /eve knows /fay >> since 2001");
    assert_eq!(out, Output::Verified(true));
    // Deleting a statement about a link, and not the link.
    assert_eq!(
        one(
            &mut g,
            "delete ?l since 2019 where << :?l /alice knows /bob >>"
        ),
        Output::Deleted(1)
    );
    assert_eq!(
        one(&mut g, "verify /alice knows /bob"),
        Output::Verified(true)
    );
    // A link node in a delete template deletes its link too.
    assert_eq!(
        one(&mut g, "delete << /alice knows /dave >> since 2024"),
        Output::Deleted(2)
    );
    assert_eq!(
        one(&mut g, "verify /alice knows /dave"),
        Output::Verified(false)
    );
    // A link node must be there: it asserts its triple.
    assert_eq!(
        one(&mut g, "verify << /alice knows /zed >>"),
        Output::Verified(false)
    );
    // lookup reports the links of link nodes too.
    let Output::Graph(graph) = one(&mut g, "lookup << /alice knows /bob >> source ?s") else {
        panic!()
    };
    let links: Vec<u64> = graph.stored().collect();
    assert!(links.contains(&to_bob), "{links:?}");
    assert_eq!(links.len(), 2);
}

#[test]
fn as_of() {
    let mut g = graph();
    one(&mut g, "insert /a knows /b , /c");
    let [a, b, c] = ["/a", "/b", "/c"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let knows = g.find_value(b"knows").unwrap();
    let to_c = g.find_link(a, knows, c).unwrap();
    let before = g.next_id() - 1;
    one(&mut g, "delete /a knows /c");
    one(&mut g, "insert /a knows /d");
    let d = g.find_by_key(b"/d").unwrap();
    let sorted = |out: &Output| {
        let mut rows = rows(out).to_vec();
        rows.sort();
        rows
    };
    // The statement sees the graph as of an id: the entry with that id
    // included.
    let out = one(
        &mut g,
        &format!("select ?x as of {before} where /a knows ?x"),
    );
    assert_eq!(sorted(&out), [[b], [c]]);
    let out = one(&mut g, "select ?x where /a knows ?x");
    assert_eq!(sorted(&out), [[b], [d]]);
    assert_eq!(
        one(&mut g, &format!("verify as of {to_c} /a knows /c")),
        Output::Verified(true)
    );
    assert_eq!(
        one(&mut g, &format!("verify as of {} /a knows /c", to_c - 1)),
        Output::Verified(false)
    );
    let Output::Graph(graph) = one(&mut g, &format!("lookup as of {before} /a knows ?x")) else {
        panic!()
    };
    let links: Vec<u64> = graph.stored().collect();
    assert!(links.contains(&to_c));
    // A group sees the graph as of an id, and the rest of the pattern now:
    // whom /a knew and no longer knows.
    let out = one(
        &mut g,
        &format!("select ?x where ( /a knows ?x ) as of {before} and not ( /a knows ?x )"),
    );
    assert_eq!(rows(&out), [[c]]);
    // Rules see the statement's time, but can't be called in an `as of`
    // group, since a rule is evaluated once per query.
    one(&mut g, "rule friend(?x) /a knows ?x");
    let out = one(
        &mut g,
        &format!("select ?x as of {before} where friend(?x)"),
    );
    assert_eq!(sorted(&out), [[b], [c]]);
    let err = execute(
        &mut g,
        &format!("select ?x where ( friend(?x) ) as of {before}"),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
    // Errors.
    for text in [
        "select ?x as of where ?x p 1",
        "verify as of x /a p 1",
        "verify as /a p 1",
    ] {
        assert!(parse(text).is_err(), "{text}");
    }
}

#[test]
fn as_of_times() {
    let mut g = graph();
    let jan = crate::times::parse_time("2026-01-01").unwrap();
    let jun = crate::times::parse_time("2026-06-01").unwrap();
    one(&mut g, "insert /a knows /b , /c");
    g.commit_at(jan).unwrap();
    one(&mut g, "delete /a knows /c");
    one(&mut g, "insert /a knows /d");
    g.commit_at(jun).unwrap();
    let [b, c, d] = ["/b", "/c", "/d"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let at = |g: &mut Graph, time: &str| {
        let out = one(g, &format!("select ?x as of {time:?} where /a knows ?x"));
        let mut rows = rows(&out).to_vec();
        rows.sort();
        rows
    };
    assert_eq!(
        at(&mut g, "2025-12-31T23:59:59.999Z"),
        Vec::<Vec<u64>>::new()
    );
    assert_eq!(at(&mut g, "2026-01-01"), [[b], [c]]);
    assert_eq!(at(&mut g, "2026-03-15T08:00"), [[b], [c]]);
    // The same instant, written with an offset.
    assert_eq!(at(&mut g, "2026-06-01T02:00+02:00"), [[b], [d]]);
    assert_eq!(at(&mut g, "2030-01-01"), [[b], [d]]);
    let out = one(
        &mut g,
        "select ?x where ( /a knows ?x ) as of \"2026-02-01\" and not ( /a knows ?x )",
    );
    assert_eq!(rows(&out), [[c]]);
    // A time prints back in UTC.
    let s = &parse("verify as of \"2026-06-01T02:00+02:00\" /a knows /c").unwrap()[0];
    assert_eq!(
        s.to_string(),
        "verify as of \"2026-06-01T00:00:00Z\" /a knows /c"
    );
    let err = parse("verify as of \"June\" /a knows /c").unwrap_err();
    assert!(err.to_string().contains("not an ISO 8601 time"), "{err}");
}

#[test]
fn optional_patterns() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann a person ; email \"ann@x\" ; name Ann .
                /ben a person ; name Ben .
                /cat a person ; email \"cat@y\" .
                \"cat@y\" verified yes",
    );
    let [ann, ben, cat] = ["/ann", "/ben", "/cat"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let [ann_mail, cat_mail, yes] =
        ["ann@x", "cat@y", "yes"].map(|d| g.find_value(d.as_bytes()).unwrap());
    let sorted = |out: &Output| {
        let mut rows = rows(out).to_vec();
        rows.sort();
        rows
    };
    let expected = {
        let mut e = vec![vec![ann, ann_mail], vec![ben, 0], vec![cat, cat_mail]];
        e.sort();
        e
    };
    // The group form, and the `-` modifier, are the same.
    for text in [
        "select ?p, ?e where ?p a person and optional ( ?p email ?e )",
        "select ?p, ?e where ?p a person . optional ( ?p email ?e )",
        "select ?p, ?e where ?p a person ; email ?e -",
    ] {
        assert_eq!(sorted(&one(&mut g, text)), expected, "{text}");
    }
    // `select *` includes the optional variables.
    let Output::Rows { vars, .. } = one(&mut g, "select * where ?p a person ; email ?e -") else {
        panic!()
    };
    assert_eq!(vars, ["p", "e"]);
    // Nested optional patterns.
    let out = one(
        &mut g,
        "select ?p, ?ok where ?p a person and optional ( ?p email ?e and optional ( ?e verified ?ok ) )",
    );
    let mut e = vec![vec![ann, 0], vec![ben, 0], vec![cat, yes]];
    e.sort();
    assert_eq!(sorted(&out), e);
    // A comparison of an unbound variable fails, and a negation of one holds.
    let out = one(
        &mut g,
        "select ?p where ?p a person ; email ?e - . ?e != \"ann@x\"",
    );
    assert_eq!(rows(&out), [[cat]]);
    let out = one(
        &mut g,
        "select ?p where ?p a person ; email ?e - and not ( ?e verified yes )",
    );
    let mut e = vec![vec![ann], vec![ben]];
    e.sort();
    assert_eq!(sorted(&out), e);
    // Templates leave out triples with unbound variables.
    assert_eq!(
        one(
            &mut g,
            "insert ?p contact ?e where ?p a person ; email ?e -"
        ),
        Output::Inserted(2)
    );
    // In rules: a contact list, with emails where known.
    one(&mut g, "rule contact(?p ?e) ?p a person ; email ?e -");
    let out = one(&mut g, "select ?p, ?e where contact(?p ?e)");
    assert_eq!(sorted(&out), expected);
    // lookup reports the links that are bound.
    let Output::Graph(graph) = one(&mut g, "lookup /ben a person ; name ?n ; email ?e -") else {
        panic!()
    };
    let links: Vec<u64> = graph.stored().collect();
    assert_eq!(links.len(), 2);
}

/// The rows of a table, as text: numbers, and entries by data or key.
fn table(g: &Graph, out: &Output) -> Vec<Vec<String>> {
    let Output::Table { rows, .. } = out else {
        panic!("{out:?}")
    };
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    &Cell::Id(id) => g
                        .key(id)
                        .or_else(|| g.data(id))
                        .map(|d| String::from_utf8_lossy(&d).into_owned())
                        .unwrap_or_else(|| format!("#{id}")),
                    other => other.to_string(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn aggregates() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann city paris ; age 41 .
                /ben city rome ; age 17 .
                /cat city paris ; age 29 .
                /dan city oslo ; age 65 .
                /eve city rome ; age 29 .
                /fay city paris",
    );
    let t = |g: &mut Graph, text: &str| {
        let out = one(g, text);
        table(g, &out)
    };
    let s = |rows: &[&[&str]]| -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .collect()
    };
    // Grouped by the variables not aggregated, ordered by a result.
    assert_eq!(
        t(
            &mut g,
            "select ?c, count(?p) as ?n by ?n desc, ?c where ?p city ?c"
        ),
        s(&[&["paris", "3"], &["rome", "2"], &["oslo", "1"]])
    );
    let Output::Table { vars, .. } = one(&mut g, "select ?c, count(?p) where ?p city ?c") else {
        panic!()
    };
    assert_eq!(vars, ["c", "count(?p)"]);
    // No groups: one row, even if nothing matches.
    assert_eq!(t(&mut g, "select count(*) where ?p city ?c"), s(&[&["6"]]));
    assert_eq!(
        t(&mut g, "select count(*) where ?p city mars"),
        s(&[&["0"]])
    );
    assert_eq!(
        t(&mut g, "select count(distinct ?c) as ?k where ?p city ?c"),
        s(&[&["3"]])
    );
    // Numbers: sum stays an integer, avg is a float, min and max are the
    // values (numerically, since they are numbers).
    assert_eq!(
        t(
            &mut g,
            "select ?c, sum(?a), avg(?a) as ?mean, min(?a), max(?a) by ?c where ?p city ?c ; age ?a"
        ),
        s(&[
            &["oslo", "65", "65", "65", "65"],
            &["paris", "70", "35", "29", "41"],
            &["rome", "46", "23", "17", "29"],
        ])
    );
    // Optional values: unbound ones are skipped, but count(*) counts rows.
    assert_eq!(
        t(
            &mut g,
            "select ?c, count(*) as ?people, count(?a) as ?aged where ?p city ?c ; age ?a - . ?c = paris"
        ),
        s(&[&["paris", "3", "2"]])
    );
    assert_eq!(
        t(&mut g, "select avg(?a) where ?p city mars ; age ?a -"),
        s(&[&[""]])
    );
    // having filters the groups, by results and grouping variables.
    assert_eq!(
        t(
            &mut g,
            "select ?c, count(?p) as ?n having ?n >= 2 and ?c != rome where ?p city ?c"
        ),
        s(&[&["paris", "3"]])
    );
    // limit applies to the groups, after ordering.
    assert_eq!(
        t(
            &mut g,
            "select ?c, max(?a) as ?oldest by ?oldest desc limit 1 where ?p city ?c ; age ?a"
        ),
        s(&[&["oslo", "65"]])
    );
    // Errors: by and having use the results' columns; having needs an
    // aggregate; only count takes *.
    for text in [
        "select ?c, count(?p) by ?p where ?p city ?c",
        "select ?c, count(?p) having ?q > 1 where ?p city ?c",
        "select ?c having ?c = rome where ?p city ?c",
    ] {
        let err = execute(&mut g, text).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{text}: {err:?}");
    }
    assert!(parse("select sum(*) where ?p city ?c").is_err());
    // desc without aggregates, too.
    let out = one(&mut g, "select ?p by ?a desc limit 2 where ?p age ?a");
    let [dan, ann] = ["/dan", "/ann"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    assert_eq!(rows(&out), [[dan], [ann]]);
}

#[test]
fn path_expressions() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann parent /ben . /ben parent /cat . /cat parent /dan .
                /eve guardian /cat .
                /ann knows /eve . /eve likes /fay . /fay likes /gus",
    );
    let k = |g: &Graph, key: &str| g.find_by_key(key.as_bytes()).unwrap();
    let [ann, ben, cat, dan, eve, fay, gus] =
        ["/ann", "/ben", "/cat", "/dan", "/eve", "/fay", "/gus"].map(|key| k(&g, key));
    let ends = |g: &mut Graph, text: &str| {
        let mut rows = rows(&one(g, text)).to_vec();
        rows.sort();
        rows.into_iter().map(|r| r[0]).collect::<Vec<u64>>()
    };
    let sorted = |mut v: Vec<u64>| {
        v.sort();
        v
    };
    // Sequences, written with spaces or as a key.
    assert_eq!(
        ends(&mut g, "select ?y where /ann (parent / parent) ?y"),
        [cat]
    );
    assert_eq!(
        ends(&mut g, "select ?y where /ann (parent/parent) ?y"),
        [cat]
    );
    assert_eq!(
        ends(&mut g, "select ?y where /ann (knows/likes/likes) ?y"),
        [gus]
    );
    // Inverse, alternatives, repetition.
    assert_eq!(ends(&mut g, "select ?x where /cat (^parent) ?x"), [ben]);
    assert_eq!(
        ends(&mut g, "select ?x where /cat (^parent | ^guardian) ?x"),
        sorted(vec![ben, eve])
    );
    assert_eq!(
        ends(&mut g, "select ?y where /ann (parent+) ?y"),
        sorted(vec![ben, cat, dan])
    );
    assert_eq!(
        ends(&mut g, "select ?y where /ann (parent*) ?y"),
        sorted(vec![ann, ben, cat, dan])
    );
    assert_eq!(
        ends(&mut g, "select ?y where /ann (parent{2,3}) ?y"),
        sorted(vec![cat, dan])
    );
    assert_eq!(
        ends(&mut g, "select ?y where /ann (parent{0,1}) ?y"),
        sorted(vec![ann, ben])
    );
    // Mixed: a step, then a repetition.
    assert_eq!(
        ends(&mut g, "select ?y where /ann (knows/likes+) ?y"),
        sorted(vec![fay, gus])
    );
    // From the object: everyone with a parent or guardian chain to /dan.
    assert_eq!(
        ends(&mut g, "select ?x where ?x ((parent | guardian)+) /dan"),
        sorted(vec![ann, ben, cat, eve])
    );
    // Both ends unknown.
    let out = one(&mut g, "select ?x, ?y where ?x (parent{3}) ?y");
    assert_eq!(rows(&out), [[ann, dan]]);
    // A path that may be empty needs an end known.
    let err = execute(&mut g, "select ?x, ?y where ?x (parent*) ?y").unwrap_err();
    assert!(err.to_string().contains("may be empty"), "{err}");
    // A chain lowers to joins, and so repeats a pair for each way to it.
    one(&mut g, "insert /ann knows /fay");
    let out = one(&mut g, "select ?y where /ann (knows / likes) ?y");
    assert_eq!(rows(&out).len(), 2);
    let out = one(&mut g, "select ?y where /ann ((knows / likes)+) ?y");
    assert_eq!(rows(&out).len(), 2);
    // Errors.
    for text in [
        "select ?y where ?x (?p+) ?y",
        "select ?y where ?x (p{3,1}) ?y",
        "select ?y where ?x (p{,2}) ?y",
        "select ?y where ?x (p/) ?y",
    ] {
        assert!(execute(&mut g, text).is_err(), "{text}");
    }
    // From a value, bare or quoted; a call is still a call.
    one(&mut g, "insert ann knows bob . bob knows cat");
    let [bob, cat] = ["bob", "cat"].map(|v| g.find_value(v.as_bytes()).unwrap());
    for text in [
        "select ?x where ann (knows+) ?x",
        "select ?x where \"ann\" (knows+) ?x",
    ] {
        assert_eq!(ends(&mut g, text), sorted(vec![bob, cat]), "{text}");
    }
    one(&mut g, "rule known(?x) ann knows ?x");
    assert_eq!(ends(&mut g, "select ?x where known(?x)"), [bob]);
    // A saved rule's reference links include its paths' predicates.
    one(&mut g, "rule ancestor(?x ?y) ?x (parent+) ?y");
    assert_eq!(rules_using(&g, b"parent").unwrap().len(), 1);
    let out = one(&mut g, "select ?y where ancestor(/ben ?y)");
    assert_eq!(rows(&out).len(), 2);
}

#[test]
fn parameters() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann knows /ben , /cat . /ben age 17 . /cat age 29 . /ben name Ben",
    );
    let [ann, ben, cat] = ["/ann", "/ben", "/cat"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let params = |pairs: &[(&str, &str)]| -> Params {
        pairs
            .iter()
            .map(|&(n, v)| (n.to_string(), Param::parse(v).unwrap()))
            .collect()
    };
    let mut s = Session::new();
    // One statement, run with different parameters.
    let statement = &parse("select ?f where $who knows ?f . ?f age ?a . ?a > $min").unwrap()[0];
    let run = |s: &mut Session, g: &mut Graph, p: &Params| match s.run_with(g, statement, p) {
        Ok(Output::Rows { rows, .. }) => rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        run(&mut s, &mut g, &params(&[("who", "/ann"), ("min", "20")])),
        [[cat]]
    );
    let mut both = run(&mut s, &mut g, &params(&[("who", "/ann"), ("min", "10")]));
    both.sort();
    assert_eq!(both, [[ben], [cat]]);
    assert_eq!(
        run(&mut s, &mut g, &params(&[("who", "#1"), ("min", "20")])),
        [[cat]]
    );
    // Values, in templates too; and in having.
    let p = params(&[("who", "/ann"), ("name", "\"Ann A\"")]);
    s.execute_with(&mut g, "insert $who name $name", &p)
        .unwrap();
    assert_eq!(
        g.data(g.property(ann, g.find_value(b"name").unwrap()).unwrap())
            .as_deref(),
        Some(&b"Ann A"[..])
    );
    let out = s
        .execute_with(
            &mut g,
            "select ?x, count(?f) as ?n having ?n >= $least where ?x knows ?f",
            &params(&[("least", "2")]),
        )
        .unwrap();
    assert!(matches!(&out[0], Output::Table { rows, .. } if rows.len() == 1));
    // Missing parameters, and rules with parameters, are errors.
    let err = s.run(&mut g, statement).unwrap_err();
    assert_eq!(err.to_string(), "$who is not given");
    let err = execute(&mut g, "rule r(?x) $who knows ?x").unwrap_err();
    assert!(err.to_string().contains("parameters"), "{err}");
    // Parsing parameters.
    assert_eq!(Param::parse("/a/b").unwrap(), Param::Key("/a/b".into()));
    assert_eq!(Param::parse("#42").unwrap(), Param::Id(42));
    assert_eq!(Param::parse("3.5").unwrap(), Param::Value("3.5".into()));
    assert_eq!(Param::parse("\"a b\"").unwrap(), Param::Value("a b".into()));
    assert!(Param::parse("?x").is_err());
    assert!(Param::parse("a b").is_err());
}

#[test]
fn ids_and_offset() {
    let mut g = graph();
    one(&mut g, "insert /a n 9 , 10 , 100 , x , 2.5");
    let a = g.find_by_key(b"/a").unwrap();
    // #id names an entry.
    let out = one(&mut g, &format!("select ?v where #{a} n ?v"));
    assert_eq!(rows(&out).len(), 5);
    assert_eq!(
        one(&mut g, &format!("verify #{a} n 9")),
        Output::Verified(true)
    );
    // An id naming no entry, inside a value, can't be inserted.
    let inside = g.find_value(b"graqlite:key").unwrap() + 3;
    let err = execute(&mut g, &format!("insert #{inside} n 1")).unwrap_err();
    assert!(err.to_string().contains("names no entry"), "{err}");
    // `by` orders numbers by value, then other data.
    let data = |g: &Graph, out: &Output| -> Vec<String> {
        rows(out)
            .iter()
            .map(|r| String::from_utf8_lossy(&g.data(r[0]).unwrap()).into_owned())
            .collect()
    };
    let out = one(&mut g, "select ?v by ?v where /a n ?v");
    assert_eq!(data(&g, &out), ["2.5", "9", "10", "100", "x"]);
    // offset skips rows, after ordering, with or without a limit.
    let out = one(&mut g, "select ?v by ?v limit 2 offset 1 where /a n ?v");
    assert_eq!(data(&g, &out), ["9", "10"]);
    let out = one(&mut g, "select ?v by ?v desc offset 3 where /a n ?v");
    assert_eq!(data(&g, &out), ["9", "2.5"]);
    let out = one(&mut g, "select ?v by ?v offset 9 where /a n ?v");
    assert!(rows(&out).is_empty());
    assert!(parse("select ?v limit 1 limit 2 where /a n ?v").is_err());
}

/// The cells of the rows of a table or of rows, as text (entries by key or
/// data).
fn texts(g: &Graph, out: &Output) -> Vec<Vec<String>> {
    match out {
        Output::Rows { rows, .. } => {
            let as_table = Output::Table {
                vars: vec![],
                rows: rows
                    .iter()
                    .map(|r| r.iter().map(|&id| Cell::of_id(id)).collect())
                    .collect(),
                holds: Default::default(),
            };
            table(g, &as_table)
        }
        out => table(g, out),
    }
}

#[test]
fn expressions() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann name Ann ; price 12 ; qty 10 ; city paris .
                /ben name Ben ; price 3.5 ; qty 2 ; city rome .
                /cat name Cat ; price 40 ; qty 3 ; city paris ; email \"cat@x\"",
    );
    let t = |g: &mut Graph, text: &str| {
        let out = one(g, text);
        texts(g, &out)
    };
    let s = |rows: &[&[&str]]| -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .collect()
    };
    // Arithmetic in select, with precedence and integer results kept.
    assert_eq!(
        t(
            &mut g,
            "select ?n, ?p * ?q as ?total, ?p + ?q * 2 by ?n where ?x name ?n ; price ?p ; qty ?q"
        ),
        s(&[
            &["Ann", "120", "32"],
            &["Ben", "7", "7.5"],
            &["Cat", "120", "46"]
        ])
    );
    // Division is a float; % is of integers; division by zero is unbound.
    assert_eq!(
        t(
            &mut g,
            "select ?q / 4, ?q % 4, ?q / 0, -?q by ?q where /ann qty ?q"
        ),
        s(&[&["2.5", "2", "", "-10"]])
    );
    // A comparison of expressions filters, inside the engine (so also in a
    // negation or an optional pattern).
    assert_eq!(
        t(
            &mut g,
            "select ?n by ?n where ?x name ?n ; price ?p ; qty ?q . ?p * ?q >= 100"
        ),
        s(&[&["Ann"], &["Cat"]])
    );
    assert_eq!(
        t(
            &mut g,
            "select ?n by ?n where ?x name ?n ; price ?p and not ( ?p * 2 > 20 )"
        ),
        s(&[&["Ben"]])
    );
    // let: a computed variable, used in filters and selected.
    assert_eq!(
        t(
            &mut g,
            "select ?n, ?total by ?total desc where ?x name ?n ; price ?p ; qty ?q
               . let ?total = ?p * ?q . ?total < 1000 . ?total > 10"
        ),
        s(&[&["Ann", "120"], &["Cat", "120"]])
    );
    // Strings: case, length, tests as subjects, concat.
    assert_eq!(
        t(
            &mut g,
            "select upper(?n), length(?n), concat(?n, \"-\", ?c) by ?n
               where ?x name ?n ; city ?c . contains(?c, \"ar\") . not starts(?n, C)"
        ),
        s(&[&["ANN", "3", "Ann-paris"]])
    );
    // Optional values: bound(), and unbound in arithmetic.
    assert_eq!(
        t(
            &mut g,
            "select ?n, bound(?e), length(?e) by ?n where ?x name ?n ; email ?e -"
        ),
        s(&[
            &["Ann", "false", ""],
            &["Ben", "false", ""],
            &["Cat", "true", "5"]
        ])
    );
    // Aggregates of expressions.
    assert_eq!(
        t(
            &mut g,
            "select ?c, sum(?p * ?q) as ?revenue by ?c where ?x city ?c ; price ?p ; qty ?q"
        ),
        s(&[&["paris", "240"], &["rome", "7"]])
    );
    // An alias of a variable.
    let Output::Table { vars, .. } = one(&mut g, "select ?n as ?who where ?x name ?n") else {
        panic!()
    };
    assert_eq!(vars, ["who"]);
    // Errors: a let can't be matched, or defined twice; a rule can't be
    // named like a function; arity.
    for text in [
        "select ?t where let ?t = 1 . ?t name ?n",
        "select ?t where ?x price ?p . let ?t = ?p . let ?t = 2",
        "rule upper(?x) ?x p 1",
        "select length() where ?x p ?y",
        "select nosuch(?x) where ?x p ?y",
    ] {
        assert!(execute(&mut g, text).is_err(), "{text}");
    }
    // Rules with expressions are saved, and evaluated, like any other.
    one(
        &mut g,
        "rule big(?x) ?x price ?p ; qty ?q . let ?t = ?p * ?q . ?t >= 100",
    );
    assert_eq!(
        Session::new().rules(&g).unwrap()[0].to_string(),
        "rule big(?x) ?x price ?p . ?x qty ?q . let ?t = ?p * ?q . ?t >= 100"
    );
    assert_eq!(
        t(&mut g, "select ?n by ?n where big(?x) . ?x name ?n"),
        s(&[&["Ann"], &["Cat"]])
    );
    // Times: when a link was made, as a number and in ISO 8601.
    let out = one(
        &mut g,
        "select iso(time(\"2026-10-31T12:00Z\")) where << :?l /ann name Ann >> . when(?l) > 0",
    );
    assert_eq!(texts(&g, &out), s(&[&["2026-10-31T12:00:00Z"]]));
}

#[test]
fn prepared_statements() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann knows /ben , /cat , /dan . /ben age 17 . /cat age 29 . /dan age 41
         . /ben likes /cat . /cat likes /dan",
    );
    let params = |pairs: &[(&str, &str)]| -> Params {
        pairs
            .iter()
            .map(|&(n, v)| (n.to_string(), Param::parse(v).unwrap()))
            .collect()
    };
    // The output, with its rows in order, as a plan of its own may differ.
    let normal = |out: Output| match out {
        Output::Rows {
            vars,
            mut rows,
            holds,
        } => {
            rows.sort();
            Output::Rows { vars, rows, holds }
        }
        Output::Table {
            vars,
            mut rows,
            holds,
        } => {
            rows.sort_by_key(|r| format!("{r:?}"));
            Output::Table { vars, rows, holds }
        }
        out => out,
    };
    let statements = [
        // Planned once: parameters in triples, comparisons and paths.
        (
            "select ?f, ?a where $who knows ?f . ?f age ?a . ?a > $min",
            true,
        ),
        (
            "select ?f where $who knows ?f . ?f age ?a . ?a != $not",
            true,
        ),
        (
            "select ?x, count(?f) as ?n where ?x knows ?f . ?f age ?a . ?a < $max",
            true,
        ),
        ("verify $who knows $whom", true),
        ("select ?y where $who (knows / likes) ?y", true),
        ("select ?y where $who likes++ ?y", true),
        (
            "select ?f where $who knows ?f and not ( ?f likes $whom )",
            true,
        ),
        // Parameters in the columns or `having`, and only there.
        (
            "select ?f, ?a + $min as ?b where $who knows ?f . ?f age ?a",
            true,
        ),
        (
            "select ?x, count(?f) as ?n having ?n >= $min where ?x knows ?f",
            true,
        ),
        (
            "select ?x, max(?a) as ?m having ?m > $max where ?x knows ?f . ?f age ?a",
            true,
        ),
        ("select ?f where ?f age ?a . ?a * 2 > $max", true),
        (
            "select ?f where ?f age ?a and not ( ?a < $min and ?a != $not )",
            true,
        ),
        ("lookup $who knows ?f", true),
        ("lookup ?x knows ?f . ?f age ?a . ?a > $min", true),
    ];
    let values = [
        // Values in the graph, compared by their data.
        params(&[
            ("who", "/ann"),
            ("whom", "/cat"),
            ("min", "17"),
            ("max", "41"),
            ("not", "29"),
        ]),
        params(&[
            ("who", "/ann"),
            ("whom", "/cat"),
            ("min", "20"),
            ("max", "30"),
            ("not", "29"),
        ]),
        params(&[
            ("who", "/ben"),
            ("whom", "/cat"),
            ("min", "10"),
            ("max", "50"),
            ("not", "17"),
        ]),
        // `21` is not a value in the graph: it compares by its data.
        params(&[
            ("who", "/ann"),
            ("whom", "/dan"),
            ("min", "21"),
            ("max", "21"),
            ("not", "x"),
        ]),
        // Neither is `/eve`, which matches nothing.
        params(&[
            ("who", "/eve"),
            ("whom", "/ann"),
            ("min", "1"),
            ("max", "99"),
            ("not", "17"),
        ]),
        params(&[
            ("who", "#1"),
            ("whom", "/cat"),
            ("min", "1"),
            ("max", "99"),
            ("not", "41"),
        ]),
    ];
    let mut s = Session::new();
    for (text, planned) in statements {
        let statement = &parse(text).unwrap()[0];
        let mut prepared = s.prepare(&g, statement).unwrap();
        assert_eq!(prepared.plan().is_some(), planned, "{text}");
        for p in &values {
            let expected = normal(s.run_with(&mut g, statement, p).unwrap());
            let got = normal(s.run_prepared(&mut g, &mut prepared, p).unwrap());
            assert_eq!(got, expected, "{text} {p:?}");
        }
        let err = s
            .run_prepared(&mut g, &mut prepared, &Params::new())
            .unwrap_err();
        assert!(err.to_string().contains("is not given"), "{err}");
    }

    // A constant not in the graph when prepared, inserted after.
    let statement = &parse("select ?f where $who mentors ?f").unwrap()[0];
    let mut prepared = s.prepare(&g, statement).unwrap();
    let ann = params(&[("who", "/ann")]);
    assert_eq!(
        rows(&s.run_prepared(&mut g, &mut prepared, &ann).unwrap()),
        &[] as &[Vec<u64>]
    );
    one(&mut g, "insert /ann mentors /ben");
    let ben = g.find_by_key(b"/ben").unwrap();
    assert_eq!(
        rows(&s.run_prepared(&mut g, &mut prepared, &ann).unwrap()),
        [[ben]]
    );

    // A rule defined after.
    let statement = &parse("select ?f where reaches($who ?f)").unwrap()[0];
    assert!(s.prepare(&g, statement).is_err());
    one(&mut g, "rule reaches(?a ?b) ?a knows ?b");
    let mut prepared = s.prepare(&g, statement).unwrap();
    assert_eq!(
        rows(&s.run_prepared(&mut g, &mut prepared, &ann).unwrap()).len(),
        3
    );
    s.execute(&mut g, "rule reaches(?a ?b) ?a mentors ?b")
        .unwrap();
    assert_eq!(
        rows(&s.run_prepared(&mut g, &mut prepared, &ann).unwrap()).len(),
        3
    );
    s.execute(&mut g, "delete rule reaches").unwrap();
    s.execute(&mut g, "rule reaches(?a ?b) ?a mentors ?b")
        .unwrap();
    assert_eq!(
        rows(&s.run_prepared(&mut g, &mut prepared, &ann).unwrap()),
        [[ben]]
    );
}

/// The links a `lookup` gave before it recorded them: for each row, the
/// link live (as of the statement) of each of its positive triples.
fn lookup_by_triples(g: &Graph, text: &str) -> BTreeSet<u64> {
    let statement = &parse(text).unwrap()[0];
    let Statement::Lookup { pattern, as_of, .. } = statement else {
        panic!("a lookup")
    };
    let mut positive = Vec::new();
    triples(pattern, &mut positive);
    let (q, names) = statement.query_with(g, &[]).unwrap();
    let mut links = BTreeSet::new();
    for row in q.run(g).unwrap() {
        let row: HashMap<&String, u64> = names.iter().zip(row).collect();
        for (s, p, o) in &positive {
            let id = |n: &Node| match n {
                Node::Var(v) | Node::Blank(v) => row.get(v).copied(),
                Node::Key(k) => g.find_by_key(k.as_bytes()),
                Node::Value(v) | Node::Symbol(v) => g.find_value(v.as_bytes()),
                Node::Id(id) => Some(*id),
                Node::Param(_) => None,
            };
            let (Some(s), Some(p), Some(o)) = (id(s), id(p), id(o)) else {
                continue;
            };
            links.extend(match as_of {
                Some(a) => g
                    .live(&g.find_links(s, p, o), Some(a.id(g)))
                    .last()
                    .copied(),
                None => g.find_link(s, p, o),
            });
        }
    }
    links
}

#[test]
fn lookup_records_its_links() {
    let mut g = graph();
    one(
        &mut g,
        "insert /ann knows /ben , /cat ; city paris ; email \"a@x\" .
                /ben knows /cat ; city rome .
                /cat knows /dan ; city paris .
                << /ann knows /ben >> since 2019",
    );
    // The last id before the delete.
    let before = g.next_id() - 1;
    one(&mut g, "delete /ann knows /cat");
    one(&mut g, "insert /ann knows /dan");
    let links = |g: &mut Graph, text: &str| -> BTreeSet<u64> {
        let Output::Graph(graph) = one(g, text) else {
            panic!("{text}")
        };
        graph.stored().collect()
    };
    // The same links as finding each positive triple's link for each row.
    for text in [
        "lookup ?x knows ?y",
        "lookup ?x knows ?y ; city paris",
        "lookup ?x knows [ city ?c ]",
        "lookup ?x city ?c and optional ( ?x email ?e )",
        "lookup ?x city paris or ?x city rome",
        "lookup ?x knows ?y and not ( ?y city rome )",
        "lookup << /ann knows ?who >> since ?year",
        "lookup ?x knows ?y . ?y knows ?z . ?x != ?z",
        &format!("lookup as of {before} /ann knows ?x"),
    ] {
        let expected = lookup_by_triples(&g, text);
        assert!(!expected.is_empty(), "{text}");
        assert_eq!(links(&mut g, text), expected, "{text}");
    }
    // A chain path's patterns are recorded too, which they weren't.
    let chain = links(&mut g, "lookup /ann (knows / knows) ?z");
    assert!(lookup_by_triples(&g, "lookup /ann (knows / knows) ?z").is_empty());
    assert_eq!(chain.len(), 2, "{chain:?}");
    // A group as of an id records the links it matched then.
    let earlier = links(
        &mut g,
        &format!("lookup ( /ann knows ?x ) as of {before} and not ( /ann knows ?x )"),
    );
    assert_eq!(earlier.len(), 1, "{earlier:?}");
}

#[test]
fn short_forms_directions_and_object_tests() {
    let mut g = graph();
    one(
        &mut g,
        "insert /joe a person ; name Joe ; mass 180 ; pet /rex .
                /ann a person ; name Ann ; mass 30 ; pet /tom , /fido .
                /bob name \"Billy Bob\" ; mass 35 .
                /f1 directed_by /woody ; starring /ann .
                /f2 directed_by /woody ; starring /joe .
                /ann parent /cat . /cat parent /dan",
    );
    let [joe, ann, bob, cat, dan, f1, f2, woody] = [
        "/joe", "/ann", "/bob", "/cat", "/dan", "/f1", "/f2", "/woody",
    ]
    .map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let select = |g: &mut Graph, text: &str| {
        let mut rows = rows(&one(g, text)).to_vec();
        rows.sort();
        rows
    };
    let graph = |g: &mut Graph, text: &str| {
        let Output::Graph(graph) = one(g, text) else {
            panic!("{text}")
        };
        graph
    };
    // A predicate with no object, and a subject alone.
    assert_eq!(
        select(&mut g, "select distinct ?p where ?p pet"),
        [[joe], [ann]]
    );
    assert_eq!(graph(&mut g, "lookup ? pet").links.len(), 3);
    let nodes = graph(&mut g, "lookup ?subjects").nodes;
    assert!(nodes.contains(&joe) && nodes.contains(&woody), "{nodes:?}");
    // `where` constrains, and its links aren't returned.
    let found = graph(&mut g, "lookup ?p where ?p mass ?m . ?m < 100");
    assert!(found.links.is_empty());
    assert_eq!(found.nodes, [ann, bob]);
    let found = graph(&mut g, "lookup ?p pet ?x where ?p name Ann");
    assert_eq!(found.links.len(), 2);
    // `<<` reverses a predicate, and `>>` chains forward from its object;
    // `;` returns to the subject.
    assert_eq!(
        select(
            &mut g,
            "select ?f, ?a where /woody << directed_by ?f >> starring ?a"
        ),
        [[f1, ann], [f2, joe]]
    );
    assert_eq!(
        select(&mut g, "select ?f where /ann << starring ?f ; pet /tom"),
        [[f1]]
    );
    // `!` reverses one predicate, of the same subject.
    assert_eq!(
        select(
            &mut g,
            "select ?f where /woody !directed_by ?f ; !starring ?x"
        ),
        Vec::<Vec<u64>>::new()
    );
    assert_eq!(
        select(&mut g, "select ?a where /dan !parent++ ?a"),
        [[ann], [cat]]
    );
    // `+` is the default.
    assert_eq!(
        select(&mut g, "select ?p where ?p name ?n + ; mass ?m"),
        [[joe], [ann], [bob]]
    );
    // Comparisons as blank nodes, and tests of objects.
    assert_eq!(
        select(&mut g, "select ?x where /ann pet [ :?x != /tom ]"),
        [[g.find_by_key(b"/fido").unwrap()]]
    );
    assert_eq!(select(&mut g, "select ?p where ?p mass [ < 32 ]"), [[ann]]);
    assert_eq!(
        select(&mut g, "select ?p where ?p mass (< 40)"),
        [[ann], [bob]]
    );
    assert_eq!(
        select(&mut g, "select ?p where ?p name (~\"Bob$\")"),
        [[bob]]
    );
    assert_eq!(
        select(&mut g, "select ?p where ?p name (prefix(\"Bil\"))"),
        [[bob]]
    );
    assert_eq!(select(&mut g, "select ?p where ?p name (Joe)"), [[joe]]);
    assert_eq!(
        select(&mut g, "select ?p, ?m where ?p mass ( :?m > 100 )"),
        [[joe, g.find_value(b"180").unwrap()]]
    );
    assert_eq!(
        select(
            &mut g,
            "select ?p where ?p name (< \"Bob\") or ?p name (> \"Bob\")"
        ),
        [[joe], [ann], [bob]]
    );
    // An invalid regular expression matches nothing.
    assert!(select(&mut g, "select ?p where ?p name (~\"(\")").is_empty());
    // In templates, objects and predicates are needed.
    assert!(execute(&mut g, "insert /a knows").is_err());
    assert!(execute(&mut g, "insert /a knows /b . /b").is_err());
    assert!(execute(&mut g, "insert /a knows /b , ").is_err());
    let _ = (cat, dan);
}

#[test]
fn lookups_derive_links() {
    use crate::query::{Derivation, ResultLink, ResultNode};
    let mut g = graph();
    one(
        &mut g,
        "insert /a knows /b . /b knows /c . /c knows /d . /a likes /d . /e knows /a",
    );
    let [a, b, c, d] = ["/a", "/b", "/c", "/d"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let knows = g.find_value(b"knows").unwrap();
    let link = |g: &Graph, s: u64, o: u64| g.find_link(s, knows, o).unwrap();
    let graph = |g: &mut Graph, text: &str| {
        let Output::Graph(graph) = one(g, text) else {
            panic!("{text}")
        };
        graph
    };
    // The derived links of a graph: subject, object, and support.
    let derived = |graph: &crate::query::ResultGraph| -> Vec<(u64, u64, Vec<u64>)> {
        let mut out: Vec<_> = graph
            .derived()
            .map(|l| match l {
                ResultLink::Derived {
                    fields: [ResultNode::Entry(s), _, ResultNode::Entry(o)],
                    support,
                    ..
                } => (*s, *o, support.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        out.sort();
        out
    };
    // A transitive path: each pair reached, with the links followed.
    let found = graph(&mut g, "lookup /a knows++ ?y");
    let (ab, bc, cd) = (link(&g, a, b), link(&g, b, c), link(&g, c, d));
    assert_eq!(
        derived(&found),
        [
            (a, b, vec![ab]),
            (a, c, vec![ab, bc]),
            (a, d, vec![ab, bc, cd])
        ]
    );
    assert_eq!(found.stored().collect::<Vec<_>>(), [ab, bc, cd]);
    // A rule: its tuples, with the links of their derivations.
    one(&mut g, "rule reach(?x ?y) ?x knows ?y");
    one(&mut g, "rule reach(?x ?y) ?x knows ?z . reach(?z ?y)");
    let found = graph(&mut g, "lookup reach(/b ?y)");
    assert_eq!(derived(&found), [(b, c, vec![bc]), (b, d, vec![bc, cd])]);
    assert!(
        found.derived().all(
            |l| matches!(l, ResultLink::Derived { by: Derivation::Rule(n), .. } if n == "reach")
        )
    );
    // A path expression: the shortest walk, inverse steps included.  (A
    // chain, such as `knows / ^likes`, is patterns: stored links only.)
    let found = graph(&mut g, "lookup /a (knows+ / ^likes) ?y");
    let ad = g.find_link(a, g.find_value(b"likes").unwrap(), d).unwrap();
    assert_eq!(derived(&found), [(a, a, vec![ab, bc, cd, ad])]);
    // Not from a where, nor from a branch of an or not taken.
    // (/e knows++ ?x reaches /a, /b, /c and /d, through /e knows /a, which
    // isn't returned.)
    let found = graph(&mut g, "lookup ?x knows ?y where /e knows++ ?x");
    assert_eq!(derived(&found), []);
    assert_eq!(found.stored().collect::<Vec<_>>(), [ab, bc, cd]);
    // The second branch's path holds for /d too, but the branch excludes it:
    // the first branch's row for /d derives nothing.
    let found = graph(
        &mut g,
        "lookup /c knows ?y or ( /a knows++ ?y and not ( /a likes ?y ) )",
    );
    assert_eq!(derived(&found), [(a, b, vec![ab]), (a, c, vec![ab, bc])]);
    // The shell shows them.
    let mut s = Session::new();
    let out = s.execute(&mut g, "lookup /c knows++ ?y").unwrap();
    assert!(matches!(&out[0], Output::Graph(graph) if graph.derived().count() == 1));
}

#[test]
fn lookups_assert_links() {
    use crate::query::{Derivation, ResultLink, ResultNode};
    let mut g = graph();
    one(
        &mut g,
        "insert /ann name \"John Smith\" . /dan name \"John Smith\" ; pet /rex , /felix",
    );
    let graph = |g: &mut Graph, text: &str| {
        let Output::Graph(graph) = one(g, text) else {
            panic!("{text}")
        };
        graph
    };
    let key = |g: &Graph, k: &str| g.find_by_key(k.as_bytes()).unwrap();
    let (ann, dan) = (key(&g, "/ann"), key(&g, "/dan"));
    // The asserted links: subject, predicate and object as text, with how
    // they were derived.
    let asserted = |g: &Graph, graph: &crate::query::ResultGraph| -> Vec<(String, Derivation)> {
        let text = |f: &ResultNode| match f {
            ResultNode::Entry(id) => match g.key(*id) {
                Some(k) => String::from_utf8(k.to_vec()).unwrap(),
                None => match g.data(*id) {
                    Some(d) => String::from_utf8(d.to_vec()).unwrap(),
                    None => "node".into(),
                },
            },
            ResultNode::Value(Cell::Text(t)) => t.clone(),
            ResultNode::Key(k) => k.clone(),
            ResultNode::Blank(n) => format!("_:{n}"),
            other => panic!("{other:?}"),
        };
        let mut out: Vec<_> = graph
            .derived()
            .map(|l| match l {
                ResultLink::Derived { fields, by, .. } => {
                    let f: Vec<String> = fields.iter().map(text).collect();
                    (f.join(" "), by.clone())
                }
                _ => unreachable!(),
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    // Each match asserts its triple, which isn't written.
    let found = graph(&mut g, "lookup ?p name \"John Smith\" . ?p is_alive true!");
    assert_eq!(
        asserted(&g, &found),
        [
            ("/ann is_alive true".to_string(), Derivation::Asserted),
            ("/dan is_alive true".to_string(), Derivation::Asserted),
        ]
    );
    assert!(matches!(
        one(&mut g, "verify /ann is_alive true"),
        Output::Verified(false)
    ));
    // An asserted link's support is the links of the row asserting it.
    let name = g.find_value(b"name").unwrap();
    let smith = g.find_value(b"John Smith").unwrap();
    assert!(found.derived().any(|l| matches!(
        l,
        ResultLink::Derived { fields: [ResultNode::Entry(s), ..], support, .. }
            if *s == ann && support == &[g.find_link(ann, name, smith).unwrap()]
    )));
    // A scope: only those with a pet, once each however many.
    let scoped = "lookup ?p name \"John Smith\" ; pet ?pet - . ?p pet_lover true! @?pet";
    assert_eq!(
        {
            let found = graph(&mut g, scoped);
            asserted(&g, &found)
        },
        [("/dan pet_lover true".to_string(), Derivation::Asserted)]
    );
    let unscoped = "lookup ?p name \"John Smith\" ; pet ?pet - . ?p pet_lover true!";
    assert_eq!(
        {
            let found = graph(&mut g, unscoped);
            asserted(&g, &found)
        }
        .len(),
        2
    );
    // A blank node is a new node for each instantiation.
    let records = "lookup ?p name \"John Smith\" ; pet ?pet - . ?p record [ animal ?pet ]! @?pet";
    let found = {
        let found = graph(&mut g, records);
        asserted(&g, &found)
    };
    assert_eq!(found.len(), 4, "{found:?}");
    let blanks: BTreeSet<&str> = found
        .iter()
        .filter_map(|(t, _)| t.split(' ').find(|w| w.starts_with("_:")))
        .collect();
    assert_eq!(blanks.len(), 2, "{found:?}");
    // An asserted triple that is stored comes back as its link.
    let found = graph(&mut g, "lookup ?p pet ?x . ?p name \"John Smith\"!");
    assert_eq!(found.derived().count(), 0);
    assert!(
        found
            .stored()
            .any(|id| id == g.find_link(dan, name, smith).unwrap())
    );
    // A rule asserts for each tuple it derives, when called.
    one(&mut g, "rule pet_owner(?p) ?p pet ? . ?p owns_pets true!");
    assert_eq!(
        {
            let found = graph(&mut g, "lookup pet_owner(?p)");
            asserted(&g, &found)
        },
        [(
            "/dan owns_pets true".to_string(),
            Derivation::Rule("pet_owner".into())
        )]
    );
    // Inserting the graph writes its asserted links, with a node for each
    // blank.
    let Output::Inserted(n) = one(&mut g, &format!("insert graph ( {records} )")) else {
        panic!()
    };
    assert!(n >= 4, "{n}");
    let rows = rows(&one(
        &mut g,
        "select ?r, ?pet where /dan record ?r . ?r animal ?pet",
    ))
    .to_vec();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0][0], rows[1][0]);
    // Where '!' isn't allowed, and what it can't assert.
    for text in [
        "select ?p where ?p name ?n . ?p x y!",
        "verify /a x y!",
        "insert /a x y!",
        "lookup ?p name ?n and optional ( ?p x y! )",
        "lookup ?p name ?n or ?p x y!",
        "lookup ?p name ?n . not ?p x y!",
        "lookup ?p name ?n . ?p not x y!",
        "lookup ?p name ?n where ?p x y!",
        "lookup ?p name ?n . ?p x ?q!",
        "lookup ?p name ?n . ?p x y! @?q",
        "lookup ?p name ?n . ?p x y! @dan",
        "lookup ?p name ?n . ?p x++ y!",
        "lookup ?p name ?n . ?p x [ :?q y z ]!",
        "lookup ?p name ?n . ?p x [ y (< 3) ]!",
    ] {
        let run = parse(text).and_then(|statements| {
            let mut session = Session::new();
            statements
                .iter()
                .try_for_each(|st| session.run(&mut g, st).map(|_| ()))
        });
        assert!(run.is_err(), "{text}");
    }
}

#[test]
fn aggregates_as_links() {
    use crate::query::{Derivation, ResultLink, ResultNode};
    let mut g = graph();
    one(
        &mut g,
        "insert /ann parent /ben . /ben parent /cat , /dan . /dan pet /rex , /felix .
         [ a marriage ; spouse /ann , /bob ; began 1990 ; ended 2000 ] .
         [ a marriage ; spouse /ann , /cy ; began 2005 ] .
         [ a marriage ; spouse /bob , /dee ; began 2003 ] .
         [ a marriage ; spouse /eve , /fay ; began 2010 ] .
         [ a marriage ; spouse /eve , /gus ; began 2010 ] .
         [ a marriage ; spouse /eve , /kim ; began 2001 ]",
    );
    let key = |g: &Graph, k: &str| g.find_by_key(k.as_bytes()).unwrap();
    // The aggregates' links of a lookup: subject, predicate, value and
    // function.
    let links = |g: &mut Graph, text: &str| -> Vec<(u64, String, String, String)> {
        let Output::Graph(graph) = one(g, text) else {
            panic!("{text}")
        };
        let mut out: Vec<_> = graph
            .derived()
            .filter_map(|l| match l {
                ResultLink::Derived {
                    fields: [ResultNode::Entry(s), p, v],
                    by: Derivation::Aggregate(f),
                    ..
                } => {
                    let text = |n: &ResultNode| match n {
                        ResultNode::Value(Cell::Text(t)) => t.clone(),
                        ResultNode::Entry(id) => {
                            String::from_utf8(g.data(*id).unwrap().to_vec()).unwrap()
                        }
                        other => panic!("{other:?}"),
                    };
                    Some((*s, text(p), text(v), f.clone()))
                }
                _ => None,
            })
            .collect();
        out.sort();
        out
    };
    let (ann, ben, dan) = (key(&g, "/ann"), key(&g, "/ben"), key(&g, "/dan"));
    let s = |t: &str| t.to_string();
    assert_eq!(
        links(
            &mut g,
            "lookup ?g parent ?p . ?p parent ?c . COUNT(?g grandkids ?c)"
        ),
        [(ann, s("grandkids"), s("2"), s("COUNT"))]
    );
    assert_eq!(
        links(
            &mut g,
            "lookup ?g parent ?p . ?p parent ?c . COUNT(?p kids ?c) @?g"
        ),
        [(ben, s("kids"), s("2"), s("COUNT"))]
    );
    // Rows, or distinct values.
    assert_eq!(
        links(&mut g, "lookup ?p pet ?x . ?p pet ?y . COUNT(?p pets ?x)"),
        [(dan, s("pets"), s("4"), s("COUNT"))]
    );
    assert_eq!(
        links(
            &mut g,
            "lookup ?p pet ?x . ?p pet ?y . COUNT(distinct ?p pets ?x)"
        ),
        [(dan, s("pets"), s("2"), s("COUNT"))]
    );
    // Rankings: each of Eve's marriages, latest first.
    let ranks = |g: &mut Graph, f: &str| -> Vec<String> {
        let mut r: Vec<String> = links(
            g,
            &format!("lookup ?m spouse /eve ; began ?b . {f}(?m rank ?b desc)"),
        )
        .into_iter()
        .map(|l| l.2)
        .collect();
        r.sort();
        r
    };
    assert_eq!(ranks(&mut g, "ORDER"), ["0", "0", "2"]);
    assert_eq!(ranks(&mut g, "DENSE_RANK"), ["0", "0", "1"]);
    assert_eq!(ranks(&mut g, "ROW_NUMBER"), ["0", "1", "2"]);
    // Each person's current spouse: their ongoing marriage ranked first,
    // and matched by rank in the same pattern.
    one(
        &mut g,
        "rule current_spouse(?person ?other) {
           ?marriage spouse ?person ; began ?from ; spouse ?other .
           ?other != ?person .
           ORDER(?marriage marriage_order ?from) @?person .
           ?marriage marriage_order (= 0) .
           not ?marriage ended
         }",
    );
    let out = one(&mut g, "select ?p, ?s by ?p where current_spouse(?p ?s)");
    let names: Vec<Vec<String>> = rows(&out)
        .iter()
        .map(|r| {
            r.iter()
                .map(|&id| String::from_utf8(g.key(id).unwrap().to_vec()).unwrap())
                .collect::<Vec<String>>()
        })
        .filter(|r| ["/ann", "/bob", "/cy", "/dee"].contains(&r[0].as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ["/ann", "/cy"],
            ["/bob", "/dee"],
            ["/cy", "/ann"],
            ["/dee", "/bob"]
        ]
        .map(|r| r.map(String::from).to_vec())
    );
    // A rank selected is a value, and inserted is written as one.
    let out = one(
        &mut g,
        "select ?r where ?m spouse /eve , /kim ; began ?b . ORDER(?m rank ?b) . ?m rank ?r",
    );
    assert_eq!(data(&g, rows(&out)), [["0"]]);
    one(
        &mut g,
        "insert ?m rank ?r where ?m spouse /kim ; began ?b . ORDER(?m rank ?b) . ?m rank ?r",
    );
    assert!(matches!(
        one(&mut g, "verify ?m spouse /kim ; rank 0"),
        Output::Verified(true)
    ));
    // Errors.
    for text in [
        "lookup ?p pet ?x and optional ( COUNT(?p pets ?x) )",
        "lookup ?p pet ?x . COUNT(?p ?q ?x)",
        "lookup ?p pet ?x . COUNT(?p pets ?x desc)",
        "lookup ?p pet ?x . COUNT(?p pets ?x) @dan",
        "lookup ?p pet ?x . COUNT(?p pets ?x) @?q",
        "lookup ?p pet ?x . COUNT(?p pets ?x) . COUNT(?p pets ?x)",
        "insert COUNT(?p pets ?x)",
        "maintain ?p n ?n into g where ?p pet ?x . COUNT(?p n ?x) . ?p n ?n",
    ] {
        let run = parse(text).and_then(|statements| {
            let mut session = Session::new();
            statements
                .iter()
                .try_for_each(|st| session.run(&mut g, st).map(|_| ()))
        });
        assert!(run.is_err(), "{text}");
    }
}

#[test]
fn vectors_are_stored_encoded_and_measured() {
    let mut g = graph();
    let mut s = Session::new();
    s.execute(
        &mut g,
        "insert /a text \"[1, 0]\" . /b text \"0.6 0.8\" . /c text \"not one\"",
    )
    .unwrap();
    s.execute(
        &mut g,
        "insert ?d embedding ?v where ?d text ?t . let ?v = vector(?t)",
    )
    .unwrap();
    let embedding = g.find_value(b"embedding").unwrap();
    let a = g.find_by_key(b"/a").unwrap();
    let v = g.property(a, embedding).unwrap();
    assert_eq!(
        g.data(v).as_deref(),
        Some(&crate::vector::encode(&[1.0, 0.0])[..])
    );
    // Text that isn't a vector is unbound, so no link is inserted for it.
    let c = g.find_by_key(b"/c").unwrap();
    assert_eq!(g.property(c, embedding), None);
    // Nothing computed is left held.
    assert_eq!(g.ephemeral_count(), 0);
    let out = s
        .execute(
            &mut g,
            "select ?d, str(?v) as ?s, distance(?v, vector(\"[0, 1]\"), \"l2\") as ?l by ?d \
             where ?d embedding ?v",
        )
        .unwrap();
    assert_eq!(
        texts(&g, &out[0]),
        vec![
            vec!["/a", "[1, 0]", "1.414214"],
            vec!["/b", "[0.6, 0.8]", "0.632456"],
        ]
    );
    // An unknown metric is unbound.
    let out = s
        .execute(
            &mut g,
            "select distance(?v, ?v, \"manhattan\") as ?m where /a embedding ?v",
        )
        .unwrap();
    assert_eq!(texts(&g, &out[0]), vec![vec![""]]);
}

/// A session with four documents' embeddings, by two writers.
fn embedded() -> (Graph, Session) {
    let mut g = graph();
    let mut s = Session::new();
    s.execute(
        &mut g,
        "insert /a text \"[1, 0]\" ; writer /ann . /b text \"[0.6, 0.8]\" ; writer /bob .
                /c text \"[0, 1]\" ; writer /ann . /d text \"[0.9, 0.1]\" ; writer /ann",
    )
    .unwrap();
    s.execute(
        &mut g,
        "insert ?x embedding ?v where ?x text ?t . let ?v = vector(?t)",
    )
    .unwrap();
    (g, s)
}

#[test]
fn nearest_finds_the_k_nearest_values() {
    let (mut g, mut s) = embedded();
    let near = |g: &mut Graph, s: &mut Session, text: &str| -> Vec<Vec<String>> {
        let out = s.execute(g, text).unwrap();
        texts(g, &out[0])
    };
    let q = "select ?x, ?d by ?d where nearest(embedding \"[1, 0.05]\" 3 ?v ?d) . ?x embedding ?v";
    assert_eq!(
        near(&mut g, &mut s, q),
        vec![
            vec!["/a", "0.001248"],
            vec!["/d", "0.001842"],
            vec!["/b", "0.360798"]
        ]
    );
    // Given as a parameter, prepared, and run with others.
    let statement =
        &parse("select ?x by ?x where nearest(embedding $q 1 ?v ?d) . ?x embedding ?v").unwrap()[0];
    let mut prepared = s.prepare(&g, statement).unwrap();
    for (q, x) in [("[0, 1]", "/c"), ("0.7 0.7", "/b"), ("[1, 0]", "/a")] {
        let params = Params::from([("q".into(), Param::Value(q.into()))]);
        let out = s.run_prepared(&mut g, &mut prepared, &params).unwrap();
        assert_eq!(texts(&g, &out), vec![vec![x]], "{q}");
    }
    // A vector bound by the pattern: each document's nearest other.
    let out = near(
        &mut g,
        &mut s,
        "select ?x, ?y by ?x where ?x embedding ?q . nearest(embedding ?q 2 ?v ?d) . \
         ?y embedding ?v . ?y != ?x",
    );
    assert_eq!(
        out,
        vec![
            vec!["/a", "/d"],
            vec!["/b", "/c"],
            vec!["/c", "/b"],
            vec!["/d", "/a"]
        ]
    );
    // A value bound already is kept if it is among the nearest.
    let out = near(
        &mut g,
        &mut s,
        "select ?x by ?x where ?x writer /ann ; embedding ?v . nearest(embedding \"[0, 1]\" 2 ?v ?d)",
    );
    assert_eq!(out, vec![vec!["/c"]]);
    // Within a graph over the database: its links are searched too.
    s.execute(
        &mut g,
        "SET extra = database:{ /e embedding \"[0.1, 0.9]\" }",
    )
    .unwrap();
    // A value given as text isn't a vector until made one: none is found
    // for /e, whose value is text.
    let out = near(
        &mut g,
        &mut s,
        "select ?x from graph extra where nearest(embedding \"[0, 1]\" 1 ?v ?d) . ?x embedding ?v",
    );
    assert_eq!(out, vec![vec!["/c"]]);
    // Nothing computed is left held: only the graph's link, key and value.
    drop(prepared);
    assert_eq!(g.ephemeral_count(), 3);
    // Mistakes.
    for (text, message) in [
        (
            "nearest(embedding \"[1, 0]\" 0 ?v ?d)",
            "k must be a number above 0",
        ),
        (
            "nearest(?p \"[1, 0]\" 1 ?v ?d) . ?x ?p ?y",
            "predicate must be a constant",
        ),
        (
            "nearest(embedding \"[1, 0]\" 1 ?v ?d manhattan)",
            "metric must be",
        ),
        ("nearest(embedding \"[1, 0]\" 1 ?v)", "nearest takes"),
        (
            "nearest(embedding \"[1, 0]\" 1 3 ?d)",
            "value must be a variable",
        ),
    ] {
        let e = s
            .execute(&mut g, &format!("select ?v where {text}"))
            .unwrap_err()
            .to_string();
        assert!(e.contains(message), "{text}: {e}");
    }
}

#[test]
fn nearest_searches_a_reader_as_of_its_commit() {
    let (mut g, mut s) = embedded();
    g.commit().unwrap();
    let r = g.reader();
    s.execute(&mut g, "insert /e text \"[0.01, 1]\"").unwrap();
    s.execute(
        &mut g,
        "insert /e embedding ?v where /e text ?t . let ?v = vector(?t)",
    )
    .unwrap();
    s.execute(&mut g, "delete /c embedding ?v where /c embedding ?v")
        .unwrap();
    let q = "select ?x where nearest(embedding \"[0, 1]\" 1 ?v ?d) . ?x embedding ?v";
    // The writer sees the new value, and not the deleted one.
    let out = s.execute(&mut g, q).unwrap();
    assert_eq!(texts(&g, &out[0]), vec![vec!["/e"]]);
    // The reader, as of its commit, the old.
    let out = Session::new().read(&r, &parse(q).unwrap()[0]).unwrap();
    assert_eq!(texts(&r, &out), vec![vec!["/c"]]);
    let out = Session::new()
        .read(
            &r,
            &parse("select ?d where nearest(embedding \"[0, 1]\" 1 ?v ?d)").unwrap()[0],
        )
        .unwrap();
    assert_eq!(texts(&r, &out), vec![vec!["0"]]);
}

#[test]
fn queries_free_the_ephemeral_values_they_compute() {
    let mut g = graph();
    let mut s = Session::new();
    for i in 1..=20 {
        s.execute(&mut g, &format!("insert /s{i} knows /x{i}"))
            .unwrap();
        // A count, a rank and a constant the graph hasn't, new each time.
        let statements = [
            "lookup ?s knows ?x . COUNT(?t friends ?x) . ?t knows ?x".to_string(),
            "lookup ?s knows ?x . RANK(?s place ?x)".to_string(),
            format!("select ?s where ?s knows \"nothing {i}\""),
        ];
        for statement in &statements {
            let out = s.execute(&mut g, statement).unwrap();
            // Held while the output is: it reads them.
            assert!(g.ephemeral_count() > 0, "{statement}");
            let _ = format!("{:?}", texts_all(&g, &out));
            drop(out);
            assert_eq!(g.ephemeral_count(), 0, "{statement}");
        }
    }
    // A graph's, while it is set: its link, and the keys not in the graph.
    s.execute(&mut g, "SET g = { /a knows /new }").unwrap();
    assert_eq!(g.ephemeral_count(), 3);
    s.execute(&mut g, "SET g = { /s1 knows /x1 }").unwrap();
    assert_eq!(g.ephemeral_count(), 1);
}

/// The text of each output's rows or links.
fn texts_all(g: &Graph, outs: &[Output]) -> Vec<String> {
    outs.iter()
        .map(|o| match o {
            Output::Graph(r) => format!("{:?}", r.links.len()),
            o => format!("{:?}", texts(g, o)),
        })
        .collect()
}

#[test]
fn graphs_and_symbols() {
    let mut g = graph();
    let mut s = Session::new();
    let run = |g: &mut Graph, s: &mut Session, text: &str| -> Result<Vec<Output>> {
        let symbols: Vec<String> = s.symbols().map(String::from).collect();
        let statements = parse_with_symbols(text, symbols)?;
        statements.iter().map(|(_, st)| s.run(g, st)).collect()
    };
    let keys = |g: &Graph, out: &[Output]| -> Vec<Vec<String>> {
        let Some(Output::Rows { rows, .. }) = out.last() else {
            panic!("{out:?}")
        };
        let mut out: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|&id| {
                        let text = g.key(id).or_else(|| g.data(id));
                        let text = text.as_deref().unwrap_or(b"?");
                        String::from_utf8(text.to_vec()).unwrap()
                    })
                    .collect()
            })
            .collect();
        out.sort();
        out
    };
    let rows = |v: &[&[&str]]| -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|x| x.to_string()).collect())
            .collect()
    };
    run(
        &mut g,
        &mut s,
        "insert /joe name \"Joe\" ; mass 180 . /ann name \"Ann\" ; mass 30 .
         /annie_hall directed_by /woody",
    )
    .unwrap();
    // Symbols: an identifier SET names is its value; a string isn't.
    let out = run(
        &mut g,
        &mut s,
        "set me = /joe . set greeting = \"Ann\" . set heavy = 100
         select ?n where me name ?n",
    );
    assert!(out.is_err(), "'.' doesn't separate statements");
    let out = run(
        &mut g,
        &mut s,
        "set me = /joe\nset greeting = \"Ann\"\nset heavy = 100\nselect ?n where me name ?n",
    )
    .unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["Joe"]]));
    let out = run(&mut g, &mut s, "select ?p where ?p name greeting").unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/ann"]]));
    let out = run(&mut g, &mut s, "select ?p where ?p name \"greeting\"").unwrap();
    assert!(keys(&g, &out).is_empty());
    let out = run(&mut g, &mut s, "select ?p where ?p mass ?m . ?m > heavy").unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/joe"]]));
    // Graphs: of their own links, layered, deleting.
    run(
        &mut g,
        &mut s,
        "set films = graph { /tiger_lily directed_by /woody }",
    )
    .unwrap();
    run(
        &mut g,
        &mut s,
        "set films = films:{ /bananas directed_by /woody }",
    )
    .unwrap();
    let out = run(
        &mut g,
        &mut s,
        "select ?f from graph films where ?f directed_by /woody",
    )
    .unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/bananas"], &["/tiger_lily"]]));
    let out = run(
        &mut g,
        &mut s,
        "select ?f from graph films:{ not /tiger_lily directed_by /woody } where ?f directed_by /woody",
    )
    .unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/bananas"]]));
    let out = run(
        &mut g,
        &mut s,
        "select ?f from graph database:{ /sleeper directed_by /woody } where ?f directed_by /woody",
    )
    .unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/annie_hall"], &["/sleeper"]]));
    let out = run(
        &mut g,
        &mut s,
        "lookup ?f directed_by /woody in graph films",
    )
    .unwrap();
    let Some(Output::Graph(found)) = out.last() else {
        panic!()
    };
    assert_eq!(found.derived().count(), 2);
    // A lookup's graph, named: its links alone.
    run(&mut g, &mut s, "set named = graph ( lookup ?p name ?n )").unwrap();
    let out = run(
        &mut g,
        &mut s,
        "select ?p, ?x from graph named where ?p ?x ?y",
    )
    .unwrap();
    assert_eq!(
        keys(&g, &out),
        rows(&[&["/ann", "name"], &["/joe", "name"]])
    );
    // A scratch data graph: written, read, never stored.
    let before = g.next_id();
    run(
        &mut g,
        &mut s,
        "data {}\ninsert /sleeper directed_by /woody . [ a note ] about /sleeper",
    )
    .unwrap();
    let out = run(&mut g, &mut s, "select ?f where ?f directed_by /woody").unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/sleeper"]]));
    let out = run(&mut g, &mut s, "delete /sleeper directed_by /woody").unwrap();
    assert!(matches!(out[..], [Output::Deleted(1)]));
    assert!(run(&mut g, &mut s, "maintain ?x a ?y into m where ?x a ?y").is_err());
    run(&mut g, &mut s, "data database").unwrap();
    assert_eq!(g.next_id(), before);
    let out = run(&mut g, &mut s, "select ?f where ?f directed_by /woody").unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["/annie_hall"]]));
    // The default graph is under every graph read.
    run(
        &mut g,
        &mut s,
        "default { directed_by gloss \"the director\" }",
    )
    .unwrap();
    let out = run(&mut g, &mut s, "select ?d where directed_by gloss ?d").unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["the director"]]));
    let out = run(
        &mut g,
        &mut s,
        "select ?d from graph films where directed_by gloss ?d",
    )
    .unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["the director"]]));
    run(&mut g, &mut s, "default {}").unwrap();
    let out = run(&mut g, &mut s, "select ?d where directed_by gloss ?d").unwrap();
    assert!(keys(&g, &out).is_empty());
    // Assumptions, until the transaction ends.
    run(&mut g, &mut s, "assume /woody born 1935").unwrap();
    let out = run(&mut g, &mut s, "select ?y where /woody born ?y").unwrap();
    assert_eq!(keys(&g, &out), rows(&[&["1935"]]));
    // A prepared statement reads them too.
    let prepared = &parse("select ?y where $who born ?y").unwrap()[0];
    let mut p = s.prepare(&g, prepared).unwrap();
    let mut params = Params::new();
    params.insert("who".into(), Param::Key("/woody".into()));
    let out = s.run_prepared(&mut g, &mut p, &params).unwrap();
    assert_eq!(keys(&g, &[out]), rows(&[&["1935"]]));
    s.end_transaction();
    let out = run(&mut g, &mut s, "select ?y where /woody born ?y").unwrap();
    assert!(keys(&g, &out).is_empty());
    // Inserting a graph writes it, a node for each of its own.
    let before = g.next_id();
    run(
        &mut g,
        &mut s,
        "insert graph films:{ [ a remake ] remakes /bananas }",
    )
    .unwrap();
    assert!(g.next_id() > before);
    let out = run(&mut g, &mut s, "select ?f where ?f directed_by /woody").unwrap();
    assert_eq!(
        keys(&g, &out),
        rows(&[&["/annie_hall"], &["/bananas"], &["/tiger_lily"]])
    );
    let out = run(
        &mut g,
        &mut s,
        "select ?r where ?r remakes /bananas ; a remake",
    )
    .unwrap();
    assert_eq!(keys(&g, &out).len(), 1);
    // Errors.
    for text in [
        "select ?x from graph nothing where ?x a ?y",
        "select ?x where ?x a films",
        "set x = graph { ?v a b }",
    ] {
        assert!(run(&mut g, &mut s, text).is_err(), "{text}");
    }
}

#[test]
fn the_rest_of_the_tour() {
    let mut g = graph();
    let mut s = Session::new();
    let run = |g: &mut Graph, s: &mut Session, text: &str| -> Result<Vec<Output>> {
        let symbols: Vec<String> = s.symbols().map(String::from).collect();
        let statements = parse_with_symbols(text, symbols)?;
        statements.iter().map(|(_, st)| s.run(g, st)).collect()
    };
    let names = |g: &Graph, out: &[Output]| -> Vec<Vec<String>> {
        let rows: Vec<Vec<String>> = match out.last() {
            Some(Output::Rows { rows, .. }) => rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|&id| {
                            let t = g.key(id).or_else(|| g.data(id));
                            let t = t.as_deref().unwrap_or(b"?");
                            String::from_utf8(t.to_vec()).unwrap()
                        })
                        .collect()
                })
                .collect(),
            Some(Output::Table { rows, .. }) => rows
                .iter()
                .map(|r| r.iter().map(|c| c.text(g).unwrap_or_default()).collect())
                .collect(),
            other => panic!("{other:?}"),
        };
        let mut rows = rows;
        rows.sort();
        rows
    };
    let rows = |v: &[&[&str]]| -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|x| x.to_string()).collect())
            .collect()
    };
    run(
        &mut g,
        &mut s,
        "insert /joe a person ; name \"Joe\" ; mass 180 ; pet /rex , /fido .
                /ann a person ; name \"Ann\" ; mass 30 ; pet /tom .
                /bob a person ; name \"Bob\" ; mass 90",
    )
    .unwrap();
    // Defaults: `-` makes predicates optional but those marked `+`.
    let out = run(
        &mut g,
        &mut s,
        "select ?p, ?x where - ?p name ?n + ; pet ?x",
    )
    .unwrap();
    assert_eq!(names(&g, &out).len(), 4);
    let out = run(&mut g, &mut s, "select ?p, ?x where + ?p name ?n ; pet ?x").unwrap();
    assert_eq!(names(&g, &out).len(), 3);
    // Nested limits: the least values, within each before.
    let out = run(&mut g, &mut s, "select ?p where ?p a person limit 2").unwrap();
    assert_eq!(names(&g, &out).len(), 2);
    let out = run(
        &mut g,
        &mut s,
        "select ?p, ?x where ?p a person and ?p pet ?x and ( ?p limit 2 . ?x limit 1 )",
    )
    .unwrap();
    assert_eq!(names(&g, &out).len(), 2);
    // TEST: equal results pass; unequal ones fail.
    let out = run(
        &mut g,
        &mut s,
        "test \"pets\" select ?x where /joe pet ?x select ?y where ?o pet ?y . ?o name \"Joe\"",
    )
    .unwrap();
    assert!(matches!(&out[..], [Output::Done(t)] if t.contains("passed")));
    assert!(
        run(
            &mut g,
            &mut s,
            "test select ?x where /joe pet ?x select ?x where /ann pet ?x"
        )
        .is_err()
    );
    // A lookup's graph equals a graph of the same links.
    assert!(
        run(
            &mut g,
            &mut s,
            "test lookup ?p mass (< 40) graph { /ann mass 30 }"
        )
        .is_ok()
    );
    assert!(
        run(
            &mut g,
            &mut s,
            "test lookup ?p mass (< 40) graph { /ann mass 31 }"
        )
        .is_err()
    );
    // Functions, with typed operators and conditions.
    run(&mut g, &mut s, "function kg(lbs) lbs / 2.2").unwrap();
    run(
        &mut g,
        &mut s,
        "function size(m) if kg(m) > 50 || m = 0 then \"big\" else \"small\"",
    )
    .unwrap();
    run(&mut g, &mut s, "function sum_int(x, y) x +integer y").unwrap();
    let out = run(
        &mut g,
        &mut s,
        "select ?p, size(?m) where ?p mass ?m . size(?m) = \"big\"",
    )
    .unwrap();
    assert_eq!(names(&g, &out), rows(&[&["/joe", "big"]]));
    let out = run(
        &mut g,
        &mut s,
        "select sum_int(\"2\", \"3.4\") as ?five where /joe a ?t",
    )
    .unwrap();
    assert_eq!(names(&g, &out), rows(&[&["5"]]));
    assert!(run(&mut g, &mut s, "select nope(1) as ?x where /joe a ?t").is_err());
    assert!(run(&mut g, &mut s, "select kg(1, 2) as ?x where /joe a ?t").is_err());
    run(&mut g, &mut s, "function loop(x) loop(x)").unwrap();
    assert!(run(&mut g, &mut s, "select loop(1) as ?x where /joe a ?t").is_err());
    // Rule parameters: bare names are variables.
    run(&mut g, &mut s, "rule owns(who, animal) { who pet animal }").unwrap();
    let out = run(&mut g, &mut s, "select ?o where owns(?o /tom)").unwrap();
    assert_eq!(names(&g, &out), rows(&[&["/ann"]]));
    // Template rules: calls in templates and graphs are their triples, a
    // new node for each call's blank node.
    run(
        &mut g,
        &mut s,
        "rule property(?p ?card ?gloss) { ?p meta [ cardinality ?card ] ! . ?p gloss ?gloss ! }",
    )
    .unwrap();
    assert!(run(&mut g, &mut s, "select ?x where property(?x ?y ?z)").is_err());
    run(
        &mut g,
        &mut s,
        "insert property(mass 1 \"in pounds\") . property(pet 0 \"a pet\")",
    )
    .unwrap();
    let out = run(
        &mut g,
        &mut s,
        "select ?p, ?c where ?p meta ?m . ?m cardinality ?c",
    )
    .unwrap();
    assert_eq!(names(&g, &out), rows(&[&["mass", "1"], &["pet", "0"]]));
    run(&mut g, &mut s, "default { property(name 1 \"a name\") }").unwrap();
    let out = run(&mut g, &mut s, "select ?gl where name gloss ?gl").unwrap();
    assert_eq!(names(&g, &out), rows(&[&["a name"]]));
    // A rule that matches too: its patterns are the insert's conditions.
    run(
        &mut g,
        &mut s,
        "rule pet_owner(?p) { ?p pet ? . ?p owns_pets true ! }",
    )
    .unwrap();
    run(
        &mut g,
        &mut s,
        "default {}\ninsert pet_owner(?p) where ?p a person",
    )
    .unwrap();
    let out = run(&mut g, &mut s, "select ?p where ?p owns_pets true").unwrap();
    assert_eq!(names(&g, &out), rows(&[&["/ann"], &["/joe"]]));
    assert!(run(&mut g, &mut s, "assume pet_owner(/joe)").is_err());
    // A template rule's triples use only its head's variables.
    assert!(run(&mut g, &mut s, "rule bad(?p) { ?p knows ?q ! }").is_err());
}

#[test]
fn keys_and_ids_in_path_expressions() {
    let mut g = graph();
    one(
        &mut g,
        "insert /a /p/knows /b . /b /p/knows /c . /c likes /a .
                /a knows /x . /x p /y . /y knows /z",
    );
    let [a, b, c, z] = ["/a", "/b", "/c", "/z"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let p_knows = g.find_by_key(b"/p/knows").unwrap();
    let select = |g: &mut Graph, text: &str| {
        let mut rows = rows(&one(g, text)).to_vec();
        rows.sort();
        rows
    };
    // A key where a step starts: first, after ^, | or ( , or after a / with
    // a space.
    assert_eq!(
        select(&mut g, "select ?x where /a (/p/knows+) ?x"),
        [[b], [c]]
    );
    assert_eq!(
        select(&mut g, "select ?x where /c (likes / /p/knows+) ?x"),
        [[b], [c]]
    );
    assert_eq!(
        select(&mut g, "select ?x where /a (^likes | /p/knows) ?x"),
        [[b], [c]]
    );
    assert_eq!(select(&mut g, "select ?x where /b (^(/p/knows)) ?x"), [[a]]);
    // An id.
    let by_id = format!("select ?x where /a (#{p_knows}+) ?x");
    assert_eq!(select(&mut g, &by_id), [[b], [c]]);
    // Right after a step, a key is more steps: values.
    assert_eq!(
        select(&mut g, "select ?x where /a (knows/p/knows) ?x"),
        [[z]]
    );
    // A key not in the graph matches nothing.
    assert!(select(&mut g, "select ?x where /a (/no/such+) ?x").is_empty());
    // As written, and saved in a rule, which still works once read again.
    let statement = &parse("select ?x where /a (^likes | /p/knows) ?x").unwrap()[0];
    assert_eq!(
        statement.to_string(),
        "select ?x where /a (^likes | /p/knows) ?x"
    );
    let mut s = Session::new();
    s.execute(&mut g, "rule known(?x ?y) ?x (/p/knows+) ?y")
        .unwrap();
    let rows = |out: &Output| rows(out).to_vec();
    let out = Session::new()
        .execute(&mut g, "select ?y where known(/a ?y)")
        .unwrap();
    let mut got = rows(&out[0]);
    got.sort();
    assert_eq!(got, [[b], [c]]);
}

#[test]
fn subgraphs_and_authors_in_graql() {
    let mut g = graph();
    let mut s = Session::new();
    let run = |g: &mut Graph, s: &mut Session, text: &str| -> Output {
        s.execute(g, text)
            .unwrap_or_else(|e| panic!("{text}: {e}"))
            .pop()
            .unwrap()
    };
    run(&mut g, &mut s, "insert /a knows /b . /b knows /c");
    let [a, b, c] = ["/a", "/b", "/c"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let knows = g.find_value(b"knows").unwrap();
    // A triple's own author and subgraph, over the statement's and the
    // session's.
    run(&mut g, &mut s, "subgraph /g/session");
    run(&mut g, &mut s, "author /people/ann");
    run(
        &mut g,
        &mut s,
        "insert /a likes /b /people/bob /g/own . /a likes /c into /g/stmt",
    );
    let [own, stmt, session, ann, bob] = [
        "/g/own",
        "/g/stmt",
        "/g/session",
        "/people/ann",
        "/people/bob",
    ]
    .map(|k| g.find_by_key(k.as_bytes()).unwrap());
    let likes = g.find_value(b"likes").unwrap();
    assert_eq!(g.subgraphs_of(a, likes, b, None), [own]);
    assert_eq!(g.subgraphs_of(a, likes, c, None), [stmt]);
    let author = |g: &Graph, s, p, o| {
        let id = g.find_link(s, p, o).unwrap();
        g.get(id).unwrap().as_link().unwrap().author()
    };
    assert_eq!(author(&g, a, likes, b), bob);
    assert_eq!(author(&g, a, likes, c), ann);
    run(&mut g, &mut s, "insert /b likes /c");
    assert_eq!(g.subgraphs_of(b, likes, c, None), [session]);
    // Deleting from a subgraph not in the graph does nothing.
    assert!(matches!(
        run(&mut g, &mut s, "delete /a likes /b from /g/none"),
        Output::Deleted(0)
    ));
    assert!(g.find_link(a, likes, b).is_some());
    // An author's keys, which need an author.
    run(&mut g, &mut s, "insert /a //tag x");
    assert!(g.find_by_key(format!("/u/{ann}/tag").as_bytes()).is_some());
    let err = Session::new()
        .execute(&mut g, "insert /a //tag x")
        .unwrap_err();
    assert!(err.to_string().contains("author"), "{err}");
    // A path within a subgraph isn't searched.
    let err = s
        .execute(&mut g, "select ?y from /g/own where /a knows++ ?y")
        .unwrap_err();
    assert!(
        err.to_string().contains("a path within a subgraph"),
        "{err}"
    );
    // A subgraph as a parameter, prepared.
    let statement = &parse("select ?o from $g where /a likes ?o").unwrap()[0];
    let mut prepared = s.prepare(&g, statement).unwrap();
    for (sg, expected) in [("/g/own", b), ("/g/stmt", c)] {
        let params = Params::from([("g".to_string(), Param::parse(sg).unwrap())]);
        let out = s.run_prepared(&mut g, &mut prepared, &params).unwrap();
        assert_eq!(rows(&out), [[expected]]);
    }
    // A lookup's links, and those a rule derives, copied into a subgraph.
    run(&mut g, &mut s, "rule reach(?x ?y) ?x knows ?y");
    run(
        &mut g,
        &mut s,
        "rule reach(?x ?y) ?x knows ?z . reach(?z ?y)",
    );
    let out = run(
        &mut g,
        &mut s,
        "insert graph ( lookup reach(/a ?y) ) into /g/reach",
    );
    assert!(matches!(out, Output::Inserted(4)), "{out:?}");
    let reached = g.find_by_key(b"/g/reach").unwrap();
    let reach = g.find_value(b"reach").unwrap();
    assert_eq!(g.subgraphs_of(a, reach, c, None), [reached]);
    assert_eq!(g.subgraphs_of(a, knows, b, None), [reached]);
}

#[test]
fn maintaining_subgraphs_in_graql() {
    let mut g = graph();
    let mut s = Session::new();
    s.execute(&mut g, "insert /a knows /b . /b knows /a . /b knows /c")
        .unwrap();
    g.commit().unwrap();
    let out = s
        .execute(
            &mut g,
            "maintain ?x knows ?y into mutual where ?x knows ?y . ?y knows ?x",
        )
        .unwrap();
    assert!(
        matches!(
            out[0],
            Output::Maintained {
                added: 2,
                removed: 0,
                ..
            }
        ),
        "{out:?}"
    );
    g.commit().unwrap();
    let pairs = |g: &mut Graph, s: &mut Session| {
        let out = s
            .execute(g, "select ?x, ?y from mutual where ?x knows ?y")
            .unwrap();
        let mut rows = rows(&out[0]).to_vec();
        rows.sort();
        rows
    };
    let [a, b, c] = ["/a", "/b", "/c"].map(|k| g.find_by_key(k.as_bytes()).unwrap());
    assert_eq!(pairs(&mut g, &mut s), [[a, b], [b, a]]);
    // Kept as the rows change, by Session::update after each commit.
    s.execute(&mut g, "insert /c knows /b . delete /a knows /b")
        .unwrap();
    g.commit().unwrap();
    let updates = s.update(&mut g).unwrap();
    assert_eq!(updates.len(), 1);
    g.commit().unwrap();
    assert_eq!(pairs(&mut g, &mut s), [[b, c], [c, b]]);
    // And then settles.
    assert!(s.update(&mut g).unwrap().is_empty());
    // A template's variables come from the pattern; no blank nodes.
    assert!(
        s.execute(&mut g, "maintain ?x knows ?z into m where ?x knows ?y")
            .is_err()
    );
    assert!(
        s.execute(&mut g, "maintain ?x knows [] into m where ?x knows ?y")
            .is_err()
    );
}

#[test]
fn sessions_on_readers() {
    // Sessions on other threads read the latest commit, while a writer
    // inserts; their rules are the graph's.
    let mut g = graph();
    let mut writer = Session::new();
    writer
        .execute(&mut g, "rule friend(?a ?b) ?a knows ?b . ?b knows ?a")
        .unwrap();
    writer
        .execute(&mut g, "insert /p0 knows /p1 . /p1 knows /p0")
        .unwrap();
    g.commit().unwrap();
    let readers = g.readers();
    std::thread::scope(|s| {
        for _ in 0..3 {
            let readers = &readers;
            s.spawn(move || {
                let mut session = Session::new();
                let statement = &parse("select ?a, ?b where friend(?a ?b)").unwrap()[0];
                let mut last = 0;
                for _ in 0..50 {
                    let r = readers.latest();
                    let Output::Rows { rows, .. } = session.read(&r, statement).unwrap() else {
                        panic!()
                    };
                    // Friends come in pairs, and only grow.
                    assert_eq!(rows.len() % 2, 0);
                    assert!(rows.len() >= last);
                    last = rows.len();
                }
                // Writing isn't reading.
                let insert = &parse("insert /x knows /y").unwrap()[0];
                assert!(session.read(&readers.latest(), insert).is_err());
            });
        }
        for i in 1..30 {
            let text = format!("insert /p{i} knows /p{0} . /p{0} knows /p{i}", i + 1);
            writer.execute(&mut g, &text).unwrap();
            g.commit().unwrap();
        }
    });
    let mut session = Session::new();
    let statement = &parse("select ?a, ?b where friend(?a ?b)").unwrap()[0];
    let Output::Rows { rows, .. } = session.read(&readers.latest(), statement).unwrap() else {
        panic!()
    };
    assert_eq!(rows.len(), 60);
}

#[test]
fn trailing_limit_is_the_statements() {
    let one = |text: &str| {
        let mut s = parse(text).unwrap();
        assert_eq!(s.len(), 1, "{text}");
        s.pop().unwrap()
    };
    // A limit, offset or `by` ending a select is the statement's, as it is
    // written before 'where'.
    for (after, before) in [
        (
            "select ?c, count(?p) as ?n by ?n desc where ?p city ?c limit 3",
            "select ?c, count(?p) as ?n by ?n desc limit 3 where ?p city ?c",
        ),
        (
            "select ?c, count(?p) as ?n where ?p city ?c by ?n desc limit 3",
            "select ?c, count(?p) as ?n by ?n desc limit 3 where ?p city ?c",
        ),
        (
            "select ?c where ?p city ?c limit 3 by ?c",
            "select ?c by ?c limit 3 where ?p city ?c",
        ),
        (
            "select ?c where ?p name ?n ; city ?c limit 3 offset 1",
            "select ?c limit 3 offset 1 where ?p name ?n ; city ?c",
        ),
        (
            "select ?c where ?p city ?c offset 2",
            "select ?c offset 2 where ?p city ?c",
        ),
        (
            "select ?c where ?p city ?c . ?c limit 3",
            "select ?c limit 3 where ?p city ?c",
        ),
        (
            "select ?c where ?p city ?c and ?p a person limit 3",
            "select ?c limit 3 where ?p city ?c and ?p a person",
        ),
        (
            "select ?c from g1 where ?p city ?c limit 3",
            "select ?c limit 3 from g1 where ?p city ?c",
        ),
        (
            "select ?c where ?p city ?c limit 3 select ?p where ?p a person",
            "select ?c limit 3 where ?p city ?c select ?p where ?p a person",
        ),
    ] {
        let a = parse(after).unwrap();
        assert_eq!(a, parse(before).unwrap(), "{after}");
    }
    // A nested limit followed by more pattern, or in parentheses, stays one.
    for text in [
        "select ?c where ?p city ?c limit 3 . ?p name ?n",
        "select ?c where ( ?p city ?c limit 3 )",
        "select ?c limit 2 where ( ?p city ?c limit 3 )",
        "select ?p, ?x where ?p a person and ?p pet ?x and ( ?p limit 2 . ?x limit 1 )",
    ] {
        let Statement::Select { pattern, .. } = one(text) else {
            panic!("{text}")
        };
        let mut limits = 0;
        fn count(p: &Pattern, n: &mut usize) {
            match p {
                Pattern::LimitValues { .. } => *n += 1,
                Pattern::And(ps) => ps.iter().for_each(|p| count(p, n)),
                _ => {}
            }
        }
        count(&pattern, &mut limits);
        assert!(limits > 0, "{text}");
    }
    // Both places at once is an error.
    for text in [
        "select ?c limit 3 where ?p city ?c limit 1",
        "select ?c offset 1 where ?p city ?c limit 1",
        "select ?c limit 3 where ?p city ?c offset 1",
        "select ?c by ?c where ?p city ?c by ?c",
        "select ?c where ?p city ?c limit 1 by ?c limit 2",
    ] {
        assert!(parse(text).is_err(), "{text}");
    }
    // A nested limit ending the pattern prints in parentheses, so that it
    // reads back the same.
    let s = one("select ?c where ( ?p city ?c limit 3 )");
    assert_eq!(s.to_string(), "select ?c where ( ?p city ?c . ?c limit 3 )");
    assert_eq!(one(&s.to_string()).to_string(), s.to_string());
}

#[test]
fn explain_shows_nested_limits() {
    let mut g = graph();
    execute(&mut g, "insert /a pet /x ; a person . /b pet /y ; a person").unwrap();
    let statement =
        &parse("select ?p, ?x where ( ?p a person and ?p pet ?x limit 1 offset 1 . ?p limit 2 )")
            .unwrap()[0];
    let (q, _) = statement.query(&g).unwrap();
    let explain = q.plan(&g).unwrap().explain();
    assert!(
        explain.contains("\nnested limit ?x 1 offset 1\n"),
        "{explain}"
    );
    assert!(
        explain.contains("\nnested limit ?p 2 for each ?x\n"),
        "{explain}"
    );
}

#[test]
fn nested_limits_apply_before_aggregating() {
    let mut g = graph();
    execute(&mut g, "insert /a a person . /b a person . /c a person").unwrap();
    let out = one(
        &mut g,
        "select count(?p) as ?n where ( ?p a person limit 2 )",
    );
    let Output::Table { rows, .. } = &out else {
        panic!("{out:?}")
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Cell::Int(2)), "{rows:?}");
}

#[test]
fn since_and_history_errors() {
    let mut g = graph();
    one(&mut g, "insert /ann knows /ben");
    one(&mut g, "rule r(?x) /ann knows ?x");
    for (text, message) in [
        ("select ?w since where /ann knows ?w", "after 'since'"),
        ("select ?w since 1 since 2 where /ann knows ?w", "twice"),
        ("select ?w history history where /ann knows ?w", "twice"),
        ("select ?w as of 1 as of 2 where /ann knows ?w", "twice"),
        ("select ?w since 1 where /ann (knows+) ?w", "a path"),
        ("select ?w where ( /ann (knows+) ?w ) history", "a path"),
        ("select ?x since 1 where r(?x)", "can't be called"),
        ("lookup ( /ann knows ?w ) history", "history"),
    ] {
        let e = match Session::new().execute(&mut g, text) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("{text} succeeded"),
        };
        assert!(e.contains(message), "{text}: {e}");
    }
    // A one-step path is a pattern.
    one(&mut g, "select ?w since 1 where /ann knows ?w");
    // `lookup` may be `since`: what it returns is live.
    one(&mut g, "lookup ( /ann knows ?w ) since 1");
}

#[test]
fn since_and_history_print() {
    for text in [
        "select ?w where ( /a b ?w ) as of 5 since 2 history",
        "select ?w where ( /a b ?w ) since \"2026-01-01T00:00:00Z\"",
    ] {
        let printed = parse(text).unwrap()[0].to_string();
        assert_eq!(printed, parse(&printed).unwrap()[0].to_string());
        assert!(printed.contains("since"), "{printed}");
    }
    let printed = parse("select ?w as of 9 since 2 history where /a b ?w").unwrap()[0].to_string();
    assert!(printed.contains("as of 9"), "{printed}");
    assert!(printed.contains("since 2 history"), "{printed}");
}

/// The statements of a script, as the shell runs them: a blank line ends a
/// chunk, and comments and backslash commands are left out.
fn script_chunks(text: &str) -> Vec<String> {
    let mut chunks = vec![String::new()];
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('\\') {
            chunks.push(String::new());
        } else if !trimmed.starts_with("--") {
            let last = chunks.last_mut().unwrap();
            last.push_str(line);
            last.push('\n');
        }
    }
    chunks.retain(|c| !c.trim().is_empty());
    chunks
}

/// Every top-level step of every query in the examples, whose estimate is
/// loosely right: the examples are small, so this catches a statistics
/// regression, not a slightly worse guess.
#[test]
fn estimates_are_near_the_actual_rows_in_the_examples() {
    const FACTOR: f64 = 20.0;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut scripts: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "graql"))
        .collect();
    scripts.sort();
    let (mut checked, mut worst) = (0, (1.0, String::new()));
    for script in scripts {
        let mut g = graph();
        let mut session = Session::new();
        for chunk in script_chunks(&std::fs::read_to_string(&script).unwrap()) {
            // What a script parses only with the shell's help is left out.
            let Ok(statements) = parse(&chunk) else {
                continue;
            };
            for statement in &statements {
                if matches!(
                    statement,
                    Statement::Select { .. } | Statement::Verify { .. } | Statement::Lookup { .. }
                ) && let Ok(rules) = session.rules(&g)
                    && let Ok((q, _)) = statement.query_with(&g, rules)
                    && let Ok(plan) = q.plan(&g)
                    && !plan.explain().contains("limit")
                    && let Ok(profile) = plan.profile(&g)
                {
                    for s in profile.steps.iter().filter(|s| s.depth == 0) {
                        let Some(estimate) = s.estimated_rows else {
                            continue;
                        };
                        let (e, a) = (estimate.max(1.0), (s.actual.rows as f64).max(1.0));
                        let off = (e / a).max(a / e);
                        checked += 1;
                        if off > worst.0 {
                            worst = (off, format!("{}: {chunk}: {s:?}", script.display()));
                        }
                    }
                }
                let _ = session.execute(&mut g, &statement.to_string());
            }
        }
    }
    assert!(checked > 100, "{checked}");
    assert!(worst.0 <= FACTOR, "{worst:?}");
}
