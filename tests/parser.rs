//! Parser tests: operator precedence, statement shapes and error positions.

use sqlengine::ast::*;
use sqlengine::sql::{parse_expr, parse_sql, parse_statement};

/// Fully parenthesised rendering of an AST expression, to make precedence visible.
fn s(e: &Expr) -> String {
    match e {
        Expr::Identifier(i) => i.value.clone(),
        Expr::CompoundIdentifier(p) => p.iter().map(|i| i.value.as_str()).collect::<Vec<_>>().join("."),
        Expr::Literal(l, _) => match l {
            Literal::Integer(v) => v.to_string(),
            Literal::Float(v) => format!("{v:?}"),
            Literal::String(v) => format!("'{v}'"),
            Literal::Boolean(b) => b.to_string(),
            Literal::Null => "NULL".into(),
            Literal::Date(d) => format!("DATE '{d}'"),
            Literal::Interval { months, days } => format!("INTERVAL({months}m,{days}d)"),
        },
        Expr::Binary { left, op, right, .. } => format!("({} {op} {})", s(left), s(right)),
        Expr::Unary { op, expr, .. } => format!("({op:?} {})", s(expr)),
        Expr::IsNull { expr, negated } => format!("({} IS {}NULL)", s(expr), if *negated { "NOT " } else { "" }),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            format!(
                "({} {}BETWEEN {} AND {})",
                s(expr),
                if *negated { "NOT " } else { "" },
                s(low),
                s(high)
            )
        }
        Expr::InList { expr, list, negated } => format!(
            "({} {}IN [{}])",
            s(expr),
            if *negated { "NOT " } else { "" },
            list.iter().map(s).collect::<Vec<_>>().join(", ")
        ),
        Expr::Like {
            expr, pattern, negated, ..
        } => format!(
            "({} {}LIKE {})",
            s(expr),
            if *negated { "NOT " } else { "" },
            s(pattern)
        ),
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            let mut out = "CASE".to_string();
            if let Some(o) = operand {
                out += &format!(" {}", s(o));
            }
            for (w, t) in branches {
                out += &format!(" WHEN {} THEN {}", s(w), s(t));
            }
            if let Some(e) = else_result {
                out += &format!(" ELSE {}", s(e));
            }
            out + " END"
        }
        Expr::Cast { expr, data_type, .. } => format!("CAST({} AS {data_type})", s(expr)),
        Expr::Function {
            name,
            args,
            distinct,
            star,
        } => {
            if *star {
                format!("{}(*)", name.value)
            } else {
                format!(
                    "{}({}{})",
                    name.value,
                    if *distinct { "DISTINCT " } else { "" },
                    args.iter().map(s).collect::<Vec<_>>().join(", ")
                )
            }
        }
        Expr::Extract { field, expr } => format!("EXTRACT({} FROM {})", field.value, s(expr)),
        Expr::Subquery(..) => "(subquery)".into(),
    }
}

fn pe(sql: &str) -> String {
    s(&parse_expr(sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

#[test]
fn arithmetic_precedence_and_associativity() {
    assert_eq!(pe("1 + 2 * 3"), "(1 + (2 * 3))");
    assert_eq!(pe("(1 + 2) * 3"), "((1 + 2) * 3)");
    assert_eq!(pe("10 - 4 - 3"), "((10 - 4) - 3)");
    assert_eq!(pe("8 / 4 / 2"), "((8 / 4) / 2)");
    assert_eq!(pe("a % 2 = 0"), "((a % 2) = 0)");
    assert_eq!(pe("'a' || 'b' || c"), "(('a' || 'b') || c)");
    assert_eq!(pe("a + b || c"), "((a + b) || c)");
}

#[test]
fn unary_operators() {
    assert_eq!(pe("-5"), "-5");
    assert_eq!(pe("-x * 2"), "((Minus x) * 2)");
    assert_eq!(pe("- -x"), "(Minus (Minus x))");
    assert_eq!(pe("-x::int"), "(Minus CAST(x AS BIGINT))");
    assert_eq!(pe("NOT a = b"), "(Not (a = b))");
    assert_eq!(pe("NOT a AND b"), "((Not a) AND b)");
}

#[test]
fn boolean_precedence() {
    assert_eq!(pe("a OR b AND c"), "(a OR (b AND c))");
    assert_eq!(pe("a AND b OR c AND d"), "((a AND b) OR (c AND d))");
    assert_eq!(pe("a = 1 AND b < 2 OR c >= 3"), "(((a = 1) AND (b < 2)) OR (c >= 3))");
}

#[test]
fn predicates() {
    assert_eq!(pe("x BETWEEN 1 AND 2 AND y"), "((x BETWEEN 1 AND 2) AND y)");
    assert_eq!(
        pe("x NOT BETWEEN a + 1 AND b * 2"),
        "(x NOT BETWEEN (a + 1) AND (b * 2))"
    );
    assert_eq!(pe("x NOT IN (1, 2, 3)"), "(x NOT IN [1, 2, 3])");
    assert_eq!(pe("x IS NOT NULL AND y IS NULL"), "((x IS NOT NULL) AND (y IS NULL))");
    assert_eq!(
        pe("name NOT LIKE 'a%' OR name LIKE '_b'"),
        "((name NOT LIKE 'a%') OR (name LIKE '_b'))"
    );
    assert_eq!(pe("a + 1 IN (b, c)"), "((a + 1) IN [b, c])");
}

#[test]
fn literals_and_special_forms() {
    assert_eq!(
        pe("DATE '1998-12-01' - INTERVAL '90' DAY"),
        "(DATE '1998-12-01' - INTERVAL(0m,90d))"
    );
    assert_eq!(pe("INTERVAL '1' YEAR"), "INTERVAL(12m,0d)");
    assert_eq!(pe("INTERVAL '3 months'"), "INTERVAL(3m,0d)");
    assert_eq!(pe("1.5e2 + .5"), "(150.0 + 0.5)");
    assert_eq!(pe("CAST(x AS DECIMAL(12, 2))"), "CAST(x AS DOUBLE)");
    assert_eq!(pe("count(*)"), "count(*)");
    assert_eq!(pe("count(DISTINCT t.a)"), "count(DISTINCT t.a)");
    assert_eq!(pe("EXTRACT(YEAR FROM d)"), "EXTRACT(year FROM d)");
    assert_eq!(pe("substring(s FROM 2 FOR 3)"), "substring(s, 2, 3)");
    assert_eq!(
        pe("CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END"),
        "CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END"
    );
    assert_eq!(pe("CASE x WHEN 1 THEN 'a' END"), "CASE x WHEN 1 THEN 'a' END");
    assert_eq!(pe("\"Mixed Case\" + \"select\""), "(Mixed Case + select)");
    assert_eq!(pe("'it''s'"), "'it's'");
}

#[test]
fn select_statement_shape() {
    let stmt = parse_statement(
        "WITH c AS (SELECT 1 AS x) SELECT DISTINCT a, b + 1 AS b1, t.* FROM t AS tt JOIN u ON tt.id = u.id LEFT JOIN v USING (id), w \
         WHERE a > 1 GROUP BY a, 2 HAVING count(*) > 1 ORDER BY a DESC NULLS FIRST, 2 LIMIT 10 OFFSET 5",
    )
    .unwrap();
    let Statement::Query(q) = stmt else {
        panic!("expected query")
    };
    assert_eq!(q.ctes.len(), 1);
    assert_eq!(q.ctes[0].name.value, "c");
    let sel = &q.body;
    assert!(sel.distinct);
    assert_eq!(sel.projection.len(), 3);
    assert!(matches!(&sel.projection[1], SelectItem::Expr { alias: Some(a), .. } if a.value == "b1"));
    assert!(matches!(&sel.projection[2], SelectItem::QualifiedWildcard(q) if q.value == "t"));
    // ((tt JOIN u) LEFT JOIN v) CROSS w
    let Some(TableRef::Join {
        left,
        kind: JoinKind::Cross,
        ..
    }) = &sel.from
    else {
        panic!("expected comma join")
    };
    let TableRef::Join {
        left: inner,
        kind: JoinKind::Left,
        constraint: JoinConstraint::Using(cols),
        ..
    } = left.as_ref()
    else {
        panic!("expected LEFT JOIN USING")
    };
    assert_eq!(cols[0].value, "id");
    assert!(matches!(
        inner.as_ref(),
        TableRef::Join {
            kind: JoinKind::Inner,
            constraint: JoinConstraint::On(_),
            ..
        }
    ));
    assert_eq!(sel.group_by.len(), 2);
    assert!(sel.having.is_some());
    assert_eq!(q.order_by.len(), 2);
    assert!(!q.order_by[0].asc);
    assert_eq!(q.order_by[0].nulls_first, Some(true));
    assert!(q.limit.is_some() && q.offset.is_some());
}

#[test]
fn implicit_aliases_and_subqueries() {
    let Statement::Query(q) = parse_statement("SELECT x total FROM (SELECT 1 AS x) sub").unwrap() else {
        panic!()
    };
    assert!(matches!(&q.body.projection[0], SelectItem::Expr { alias: Some(a), .. } if a.value == "total"));
    assert!(matches!(&q.body.from, Some(TableRef::Subquery { alias: Some(a), .. }) if a.value == "sub"));
    // A reserved word is never an implicit alias.
    let Statement::Query(q) = parse_statement("SELECT a FROM t WHERE a = 1").unwrap() else {
        panic!()
    };
    assert!(matches!(&q.body.from, Some(TableRef::Table { alias: None, .. })));
}

#[test]
fn other_statements() {
    let stmts = parse_sql(
        "CREATE TABLE t (a INT NOT NULL, b VARCHAR(10), c DECIMAL(15,2), d DATE);
         INSERT INTO t (a, b) VALUES (1, 'x'), (2, NULL);
         INSERT INTO t SELECT * FROM t;
         EXPLAIN ANALYZE SELECT * FROM t;
         CREATE TABLE u AS SELECT a FROM t;
         DROP TABLE IF EXISTS u;
         SHOW TABLES; DESCRIBE t;",
    )
    .unwrap();
    assert_eq!(stmts.len(), 8);
    assert!(matches!(&stmts[0], Statement::CreateTable { columns, .. } if columns.len() == 4));
    assert!(
        matches!(&stmts[1], Statement::Insert { columns: Some(c), source: InsertSource::Values(rows), .. } if c.len() == 2 && rows.len() == 2)
    );
    assert!(matches!(
        &stmts[2],
        Statement::Insert {
            source: InsertSource::Query(_),
            ..
        }
    ));
    assert!(matches!(&stmts[3], Statement::Explain { analyze: true, .. }));
    assert!(matches!(&stmts[5], Statement::DropTable { if_exists: true, .. }));
}

#[test]
fn errors_report_positions() {
    let e = parse_statement("SELECT a FROM t WHERE").unwrap_err();
    assert_eq!(e.message, "expected an expression, found end of input");
    assert_eq!((e.span.unwrap().line, e.span.unwrap().col), (1, 22));

    let e = parse_statement("SELECT a b c FROM t").unwrap_err();
    assert!(e.message.contains("found \"c\""), "{}", e.message);
    assert_eq!(e.span.unwrap().col, 12);

    let e = parse_statement("SELECT *\nFROM t\nWHERE a = (1 + 2").unwrap_err();
    assert_eq!(e.span.unwrap().line, 3);
    assert!(e.message.contains("expected \")\""));

    let e = parse_statement("SELECT * FROM t RIGHT JOIN u ON t.a = u.a").unwrap_err();
    assert!(e.message.contains("only INNER, LEFT and CROSS"));
    assert_eq!(e.span.unwrap().col, 17);

    let e = parse_statement("SELECT 1 +").unwrap_err();
    assert!(e.message.starts_with("expected an expression"));

    let e = parse_statement("SELEC 1").unwrap_err();
    assert!(e.message.starts_with("expected a statement"));

    let rendered = parse_statement("SELECT a FROM t WHERE x = = 1")
        .unwrap_err()
        .render("SELECT a FROM t WHERE x = = 1");
    assert!(rendered.ends_with("                          ^"), "{rendered}");
}
