// SPDX-License-Identifier: BUSL-1.1
use crate::ast::*;
use nom::{
    IResult,
    branch::alt,
    bytes::complete::{tag, tag_no_case, take_while1},
    character::complete::{char, multispace0, one_of},
    combinator::{map, not, opt, peek, recognize, value},
    multi::separated_list1,
    sequence::{delimited, pair, preceded, tuple},
};
use xyzdb_core::error::XyzError;

/// Turn a `nom` failure into a message meant for whoever typed the statement.
///
/// `format!("{e}")` on a `nom::Err` renders the inner error's `Debug`, which put
/// `Parsing Error: Error { input: "AUTOLINK", code: Tag }` in front of users: it
/// names a combinator instead of saying what was expected, and its shape is a
/// dependency's internal type, so a `nom` upgrade could change it. Statements
/// that have a hand-written message still produce it — this only replaces the
/// fallback, so the leak cannot come back through a site nobody rewrote.
///
/// Giving every argument parser an expected-token message is the real fix and is
/// tracked in `KNOWN-ISSUES.md`; this closes the leak in the meantime.
fn parse_failure(e: nom::Err<nom::error::Error<&str>>) -> XyzError {
    let rest = match &e {
        nom::Err::Error(inner) | nom::Err::Failure(inner) => inner.input.trim(),
        nom::Err::Incomplete(_) => "",
    };
    if rest.is_empty() {
        return XyzError::Parse(
            "statement ends where more input was expected — check the statement's \
             grammar in docs/xytalk-spec.md"
                .into(),
        );
    }
    // Enough to locate the problem, bounded so a pasted payload cannot echo back
    // in full.
    let shown: String = rest.chars().take(40).collect();
    let ellipsis = if rest.chars().count() > 40 { "…" } else { "" };
    XyzError::Parse(format!(
        "could not parse from: '{shown}{ellipsis}' — check the statement's grammar \
         in docs/xytalk-spec.md"
    ))
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Parse a single xyTalk statement. Supports pipelines with `|`.
pub fn parse(input: &str) -> Result<Statement, XyzError> {
    let clean = strip_comments(input);
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return Err(XyzError::Parse("Empty input".into()));
    }

    parse_segment(trimmed)
}

/// Parse one already-trimmed segment: a single statement or a `|` pipeline.
/// `CREATE GHOST` is a saved query — it owns its `|` pipes (`… | GROUP BY … |
/// AGGREGATE … | TAKE BY …`), so it is parsed whole rather than split as a
/// record pipeline (which must start with FIND/SCAN).
fn parse_segment(trimmed: &str) -> Result<Statement, XyzError> {
    if trimmed.to_uppercase().starts_with("CREATE GHOST") {
        return parse_single_statement(trimmed);
    }
    let segments: Vec<&str> = split_pipeline(trimmed);
    if segments.len() == 1 {
        parse_single_statement(segments[0])
    } else {
        parse_pipeline(&segments)
    }
}

/// Parse multiple xyTalk statements separated by `;`.
/// Returns one Statement per segment. Empty segments are skipped.
pub fn parse_multi(input: &str) -> Result<Vec<Statement>, XyzError> {
    let clean = strip_comments(input);
    let mut stmts = Vec::new();
    for segment in clean.split(';') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        stmts.push(parse_segment(trimmed)?);
    }
    if stmts.is_empty() {
        return Err(XyzError::Parse("Empty input".into()));
    }
    Ok(stmts)
}

// ─── Pipeline ────────────────────────────────────────────────────────────────

fn split_pipeline(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut last = 0;
    let mut in_string = false;
    let bytes = input.as_bytes();

    for i in 0..bytes.len() {
        match bytes[i] {
            b'"' if !in_string => in_string = true,
            b'"' if in_string => in_string = false,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => depth -= 1,
            b'|' if !in_string && depth == 0 => {
                parts.push(input[last..i].trim());
                last = i + 1;
            }
            _ => {}
        }
    }
    parts.push(input[last..].trim());
    parts
}

fn parse_pipeline(segments: &[&str]) -> Result<Statement, XyzError> {
    let mut steps = Vec::with_capacity(segments.len());

    for (i, seg) in segments.iter().enumerate() {
        if i == 0 {
            // First segment must be FIND or SCAN
            let stmt = parse_single_statement(seg)?;
            match stmt {
                Statement::Find(f) => steps.push(PipelineStep::Find(f)),
                Statement::Scan(s) => steps.push(PipelineStep::Scan(s)),
                Statement::ScanGhost(s) => steps.push(PipelineStep::ScanGhost(s)),
                _ => {
                    return Err(XyzError::Parse(
                        "Pipeline must start with FIND, SCAN, or SCAN GHOST".into(),
                    ));
                }
            }
        } else {
            steps.push(parse_pipeline_step(seg)?);
        }
    }

    Ok(Statement::Pipeline(steps))
}

fn parse_pipeline_step(input: &str) -> Result<PipelineStep, XyzError> {
    let trimmed = input.trim();
    let upper = trimmed.to_uppercase();

    if upper.starts_with("PULL") {
        let (_, pull) = parse_pull_step(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Pull(pull))
    } else if upper.starts_with("SET") {
        let (_, set) = parse_set_step(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Set(set))
    } else if upper.starts_with("DELETE") {
        let (_, del) = parse_delete_step(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Delete(del))
    } else if upper.starts_with("AGGREGATE") {
        let (_, funcs) = parse_aggregate(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Aggregate(funcs))
    } else if upper.starts_with("GROUP") {
        let (_, fields) = parse_group_by(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::GroupBy(fields))
    } else if upper.starts_with("NEAREST") {
        let (_, near) = parse_nearest(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Nearest(near))
    } else if upper.starts_with("FOLLOW") {
        let (_, f) = parse_follow(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Follow(f))
    } else if upper.starts_with("TAKE") || upper.starts_with("TOP") {
        let (_, top) = parse_take(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Top(top))
    } else if upper.starts_with("SHAPE") {
        let (_, shape) = parse_shape_step(trimmed).map_err(|e| parse_failure(e))?;
        Ok(PipelineStep::Shape(shape))
    } else {
        Err(XyzError::Parse(format!(
            "Unknown pipeline step: '{trimmed}'. Expected PULL, SET, DELETE, AGGREGATE, GROUP BY, TAKE, SHAPE, NEAREST, or FOLLOW"
        )))
    }
}

/// `TAKE <n> [BY <metric> [DESC|ASC]]` — the canonical top-N / truncate step.
/// `TOP` is accepted as a live alias (same node). With `BY`, `<metric>` is a
/// declared aggregate: a function form (`sum(monto)`, `count()`) or an `AS`
/// alias; default order is DESC (the highest N). Without `BY`, it truncates the
/// stream to the first `n` items (the pipeline form of `LIMIT`).
fn parse_take(input: &str) -> IResult<&str, TopStmt> {
    let (input, _) = ws(alt((kw("TAKE"), kw("TOP"))))(input)?;
    let (input, digits) = ws(take_while1(|c: char| c.is_ascii_digit()))(input)?;
    let n: u64 = digits.parse().map_err(|_| {
        nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Digit))
    })?;
    let (input, by) = opt(preceded(
        ws(kw("BY")),
        alt((
            map(parse_aggregate_func, TopBy::Metric),
            map(ws(identifier), |s: &str| TopBy::Alias(s.to_string())),
        )),
    ))(input)?;
    let (input, descending) = opt(alt((
        map(ws(kw("DESC")), |_| true),
        map(ws(kw("ASC")), |_| false),
    )))(input)?;
    Ok((
        input,
        TopStmt {
            n,
            by,
            descending: descending.unwrap_or(true),
        },
    ))
}

fn parse_group_by(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = ws(kw("GROUP"))(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, fields) = separated_list1(ws(char(',')), ws(identifier))(input)?;
    Ok((input, fields.into_iter().map(|s| s.to_string()).collect()))
}

/// `SHAPE {field1, field2, …}` — projection. Braces mirror `PUT {…}`; contents
/// are bare field names, not assignments.
fn parse_shape_step(input: &str) -> IResult<&str, ShapeStmt> {
    let (input, _) = ws(kw("SHAPE"))(input)?;
    let (input, _) = ws(char('{'))(input)?;
    let (input, fields) = separated_list1(ws(char(',')), ws(identifier))(input)?;
    let (input, _) = ws(char('}'))(input)?;
    Ok((
        input,
        ShapeStmt {
            fields: fields.into_iter().map(|s| s.to_string()).collect(),
        },
    ))
}

// ─── Single Statement Dispatch ───────────────────────────────────────────────

fn parse_single_statement(input: &str) -> Result<Statement, XyzError> {
    let trimmed = input.trim();
    let upper = trimmed.to_uppercase();

    if upper.starts_with("PUT BATCH") || upper.starts_with("PUT  BATCH") {
        let (_, stmt) = parse_put_batch(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::PutBatch(stmt))
    } else if upper.starts_with("PUT") {
        let (_, stmt) = parse_put(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Put(stmt))
    } else if upper.starts_with("FIND") {
        let (rest, stmt) = parse_find(trimmed).map_err(|e| parse_failure(e))?;
        // P1: FIND is AND-only by design (anchor/gravity fast path). OR/NOT needs
        // a traversal — teach the fix instead of silently dropping the rest of the
        // predicate, which is what the un-consumed tail used to do.
        let tail = rest.trim_start();
        if !tail.is_empty() {
            let word: String = tail
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_uppercase();
            if word == "OR" || word == "NOT" {
                return Err(XyzError::Parse(
                    "FIND is AND-only (anchor/gravity fast path); OR/NOT needs a \
                     traversal — use SCAN"
                        .into(),
                ));
            }
            return Err(XyzError::Parse(format!(
                "unexpected trailing input in FIND: '{tail}'"
            )));
        }
        Ok(Statement::Find(stmt))
    } else if upper.starts_with("PULL") {
        let (_, stmt) = parse_pull_full(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Pull(stmt))
    } else if upper.starts_with("SCAN GHOST") {
        let (_, stmt) = parse_scan_ghost(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::ScanGhost(stmt))
    } else if upper.starts_with("SCAN") {
        let (_, stmt) = parse_scan(trimmed).map_err(|e| parse_failure(e))?;
        // P5: ORDER BY must be bounded — an unbounded sort is a footgun that used
        // to parse and fail later in the engine. Fail here with a message that
        // teaches the fix.
        if stmt.order_by.is_some() && stmt.limit.is_none() {
            return Err(XyzError::Parse(
                "ORDER BY requires LIMIT to bound the sort — add `LIMIT n`, \
                 or use `| TAKE n BY <field>` for a top-N"
                    .into(),
            ));
        }
        Ok(Statement::Scan(stmt))
    } else if upper.starts_with("CREATE GHOST") {
        let (_, stmt) = parse_create_ghost(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::CreateGhost(stmt))
    } else if upper.starts_with("REFRESH GHOST") {
        let (_, name) = parse_refresh_ghost(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::RefreshGhost(name))
    } else if upper.starts_with("DROP GHOST") {
        let (_, name) = parse_drop_ghost(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::DropGhost(name))
    } else if upper.starts_with("SET") {
        let (_, stmt) = parse_set_full(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Set(stmt))
    } else if upper.starts_with("DELETE") {
        let (_, stmt) = parse_delete_full(trimmed).map_err(|e| parse_failure(e))?;
        // P7: DELETE requires a WHERE — a WHERE-less DELETE used to empty the
        // whole target silently. Teach the explicit total-delete verb.
        if stmt.filter_expr.is_none() {
            return Err(XyzError::Parse(
                "DELETE requires WHERE — add a predicate, or use `PURGE \"lobe\"` \
                 to empty a whole lobe"
                    .into(),
            ));
        }
        Ok(Statement::Delete(stmt))
    } else if upper.starts_with("PURGE") {
        let (_, stmt) = parse_purge(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Purge(stmt))
    } else if upper.starts_with("FETCH") {
        let (_, stmt) = parse_fetch(trimmed).map_err(|e| parse_failure(e))?;
        // FETCH requires a WHERE (the shared co-location key); a keyless FETCH
        // would pull whole lobes. Teach the fix rather than surprise the caller.
        if stmt.filter_expr.is_none() {
            return Err(XyzError::Parse(
                "FETCH requires WHERE — add the shared predicate (e.g. WHERE rfc = \"X\") \
                 that selects the co-located records across the lobes"
                    .into(),
            ));
        }
        // AS names, when given, must match the lobes one-for-one.
        if let Some(names) = &stmt.names
            && names.len() != stmt.lobes.len()
        {
            return Err(XyzError::Parse(format!(
                "FETCH AS lists {} name(s) for {} lobe(s) — give one section name per lobe",
                names.len(),
                stmt.lobes.len()
            )));
        }
        Ok(Statement::Fetch(stmt))
    } else if upper.starts_with("LINK") {
        let (_, stmt) = parse_link(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Link(stmt))
    } else if upper.starts_with("ANALYZE") {
        let (_, name) = parse_analyze(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Analyze(name))
    } else if let Some(rest) = upper.strip_prefix("BULKMODE") {
        let rest = rest.trim();
        if rest.starts_with("ON") {
            Ok(Statement::BulkMode(true))
        } else if rest.starts_with("OFF") {
            Ok(Statement::BulkMode(false))
        } else {
            Err(XyzError::Parse(
                "Expected BULKMODE ON or BULKMODE OFF".into(),
            ))
        }
    } else if upper.starts_with("COMPACT") {
        Ok(Statement::Compact)
    } else if upper.starts_with("SCRUB") {
        Ok(Statement::Scrub)
    } else if upper.starts_with("MIGRATE") {
        let rest = trimmed[7..].trim();
        if rest.is_empty() {
            Ok(Statement::Migrate(None))
        } else {
            let name = rest.trim_matches('"').to_string();
            Ok(Statement::Migrate(Some(name)))
        }
    } else if upper.starts_with("PIN") && !upper.starts_with("PIPELINE") {
        let (_, stmt) = parse_pin(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Pin(stmt))
    } else if upper.starts_with("UNPIN") {
        let (_, stmt) = parse_unpin(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Unpin(stmt))
    } else if upper.starts_with("AUTOANCHOR") {
        let (_, stmt) = parse_autoanchor_apply(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::AutoAnchorApply(stmt))
    } else if upper.starts_with("ANCHOR") {
        let (_, stmt) = parse_anchor(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Anchor(stmt))
    } else if upper.starts_with("GRAVITY") {
        let (_, stmt) = parse_gravity(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Gravity(stmt))
    } else if upper.starts_with("VECTOR") {
        let (_, stmt) = parse_vector(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Vector(stmt))
    } else if upper.starts_with("SATELLITE") {
        let (_, stmt) = parse_satellite(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Satellite(stmt))
    } else if upper.starts_with("LOBE") {
        let (_, stmt) = parse_lobe(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Lobe(stmt))
    } else if upper.starts_with("INCACHE") {
        let (_, stmt) = parse_incache(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::InCache(stmt))
    } else if upper.starts_with("OUTCACHE") {
        let (_, lobe) = parse_outcache(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::OutCache(lobe))
    } else if upper.starts_with("SHOW") {
        let (_, stmt) = parse_show(trimmed).map_err(|e| parse_failure(e))?;
        Ok(Statement::Show(stmt))
    } else {
        Err(XyzError::Parse(format!(
            "Unknown command: '{}'",
            trimmed.split_whitespace().next().unwrap_or(trimmed)
        )))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn strip_comments(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            // Find -- outside of quoted strings
            let mut in_string = false;
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    in_string = !in_string;
                } else if !in_string
                    && i + 1 < bytes.len()
                    && bytes[i] == b'-'
                    && bytes[i + 1] == b'-'
                {
                    return &line[..i];
                }
                i += 1;
            }
            line
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn ws<'a, F, O>(inner: F) -> impl FnMut(&'a str) -> IResult<&'a str, O>
where
    F: FnMut(&'a str) -> IResult<&'a str, O>,
{
    delimited(multispace0, inner, multispace0)
}

fn kw<'a>(keyword: &'static str) -> impl FnMut(&'a str) -> IResult<&'a str, &'a str> {
    tag_no_case(keyword)
}

fn quoted_string(input: &str) -> IResult<&str, String> {
    let (input, _) = char('"')(input)?;
    let mut result = String::new();
    let mut chars = input.chars();
    let mut rest_pos = 0;
    loop {
        match chars.next() {
            None => {
                return Err(nom::Err::Error(nom::error::Error::new(
                    input,
                    nom::error::ErrorKind::Char,
                )));
            }
            Some('\\') => {
                rest_pos += 1;
                match chars.next() {
                    Some('"') => {
                        result.push('"');
                        rest_pos += 1;
                    }
                    Some('\\') => {
                        result.push('\\');
                        rest_pos += 1;
                    }
                    Some(c) => {
                        result.push('\\');
                        result.push(c);
                        rest_pos += c.len_utf8();
                    }
                    None => {
                        return Err(nom::Err::Error(nom::error::Error::new(
                            input,
                            nom::error::ErrorKind::Char,
                        )));
                    }
                }
            }
            Some('"') => {
                rest_pos += 1;
                return Ok((&input[rest_pos..], result));
            }
            Some(c) => {
                result.push(c);
                rest_pos += c.len_utf8();
            }
        }
    }
}

fn identifier(input: &str) -> IResult<&str, &str> {
    take_while1(|c: char| c.is_alphanumeric() || c == '_' || c == '.')(input)
}

/// Max nesting depth for List/Map literals to prevent stack overflow.
const MAX_LITERAL_DEPTH: u8 = 16;

fn parse_literal(input: &str) -> IResult<&str, Literal> {
    parse_literal_depth(input, 0)
}

fn parse_literal_depth(input: &str, depth: u8) -> IResult<&str, Literal> {
    if depth > MAX_LITERAL_DEPTH {
        return Err(nom::Err::Failure(nom::error::Error::new(
            input,
            nom::error::ErrorKind::TooLarge,
        )));
    }
    // Fast path: a literal that begins with a digit or '-' is unambiguously a
    // number — no other literal form starts that way (list '[', map '{',
    // timestamp '@', lid 'LID', null/bool are alpha, string '"'). Going straight
    // to the number parser skips the 7-alternative `alt()` below, which otherwise
    // runs (and fails 7 times) for EVERY element of a numeric vector. With
    // 768-float embeddings this is the dominant ingest-parse cost (≈73% of V1
    // ingest CPU). Transparent: identical result, no grammar change.
    if let Some(&b) = input.as_bytes().first() {
        if b.is_ascii_digit() || b == b'-' {
            return parse_number_literal(input);
        }
        // S1: `$name` / `$1` is a bound-parameter placeholder, unambiguous start.
        if b == b'$' {
            return parse_param_literal(input);
        }
    }
    alt((
        |i| parse_list_literal(i, depth),
        |i| parse_map_literal(i, depth),
        parse_timestamp_literal,
        parse_lid_literal,
        parse_null_literal,
        parse_bool_literal,
        parse_string_literal,
        parse_number_literal,
    ))(input)
}

fn parse_list_literal(input: &str, depth: u8) -> IResult<&str, Literal> {
    let (input, _) = char('[')(input)?;
    let (input, _) = multispace0(input)?;
    // Handle empty list
    if let Ok((input, _)) = char::<_, nom::error::Error<&str>>(']')(input) {
        return Ok((input, Literal::List(vec![])));
    }
    let (input, first) = parse_literal_depth(input.trim(), depth + 1)?;
    let mut items = vec![first];
    let mut remaining = input;
    loop {
        let trimmed = remaining.trim_start();
        if let Ok((rest, _)) = char::<_, nom::error::Error<&str>>(']')(trimmed) {
            remaining = rest;
            break;
        }
        let (rest, _) = char(',')(trimmed).map_err(|_: nom::Err<nom::error::Error<&str>>| {
            nom::Err::Failure(nom::error::Error::new(trimmed, nom::error::ErrorKind::Char))
        })?;
        let (rest, item) = parse_literal_depth(rest.trim(), depth + 1)?;
        items.push(item);
        remaining = rest;
    }
    Ok((remaining, Literal::List(items)))
}

fn parse_map_literal(input: &str, depth: u8) -> IResult<&str, Literal> {
    let (input, _) = char('{')(input)?;
    let (input, _) = multispace0(input)?;
    // Handle empty map
    if let Ok((input, _)) = char::<_, nom::error::Error<&str>>('}')(input) {
        return Ok((input, Literal::Map(vec![])));
    }
    let (input, first_key) = identifier(input.trim())?;
    let (input, _) = ws(char(':'))(input)?;
    let (input, first_val) = parse_literal_depth(input.trim(), depth + 1)?;
    let mut pairs = vec![(first_key.to_string(), first_val)];
    let mut remaining = input;
    loop {
        let trimmed = remaining.trim_start();
        if let Ok((rest, _)) = char::<_, nom::error::Error<&str>>('}')(trimmed) {
            remaining = rest;
            break;
        }
        let (rest, _) = char(',')(trimmed).map_err(|_: nom::Err<nom::error::Error<&str>>| {
            nom::Err::Failure(nom::error::Error::new(trimmed, nom::error::ErrorKind::Char))
        })?;
        let rest = rest.trim_start();
        let (rest, key) = identifier(rest)?;
        let (rest, _) = ws(char(':'))(rest)?;
        let (rest, val) = parse_literal_depth(rest.trim(), depth + 1)?;
        pairs.push((key.to_string(), val));
        remaining = rest;
    }
    Ok((remaining, Literal::Map(pairs)))
}

fn parse_null_literal(input: &str) -> IResult<&str, Literal> {
    let (input, _) = tag_no_case("null")(input)?;
    // Ensure not part of a longer identifier (e.g. "nullable")
    let (input, _) = not(peek(take_while1(|c: char| c.is_alphanumeric() || c == '_')))(input)?;
    Ok((input, Literal::Null))
}

fn parse_string_literal(input: &str) -> IResult<&str, Literal> {
    map(quoted_string, Literal::Text)(input)
}

fn parse_bool_literal(input: &str) -> IResult<&str, Literal> {
    alt((
        value(Literal::Bool(true), kw("true")),
        value(Literal::Bool(false), kw("false")),
    ))(input)
}

fn parse_number_literal(input: &str) -> IResult<&str, Literal> {
    let (input, num_str) = recognize(tuple((
        opt(char('-')),
        take_while1(|c: char| c.is_ascii_digit()),
        opt(pair(char('.'), take_while1(|c: char| c.is_ascii_digit()))),
        // Optional scientific-notation exponent (e.g. `5.7e-05`, `1E9`): the
        // f32 components of embeddings serialize this way, and `f64::parse`
        // already accepts it — the grammar just has to recognize the suffix.
        opt(tuple((
            one_of("eE"),
            opt(one_of("+-")),
            take_while1(|c: char| c.is_ascii_digit()),
        ))),
    )))(input)?;

    if num_str.contains('.') || num_str.contains('e') || num_str.contains('E') {
        let f: f64 = num_str.parse().map_err(|_| {
            nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Float))
        })?;
        Ok((input, Literal::Float(f)))
    } else {
        let i: i64 = num_str.parse().map_err(|_| {
            nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Digit))
        })?;
        Ok((input, Literal::Int(i)))
    }
}

fn parse_timestamp_literal(input: &str) -> IResult<&str, Literal> {
    let (input, _) = char('@')(input)?;
    let (input, s) = quoted_string(input)?;
    Ok((input, Literal::Timestamp(s)))
}

fn parse_lid_literal(input: &str) -> IResult<&str, Literal> {
    let (input, _) = kw("LID")(input)?;
    let (input, _) = multispace0(input)?;
    let (input, _) = char('(')(input)?;
    let (input, s) = quoted_string(input)?;
    let (input, _) = char(')')(input)?;
    Ok((input, Literal::Lid(s)))
}

fn parse_filter_op(input: &str) -> IResult<&str, FilterOp> {
    alt((
        value(FilterOp::Neq, tag("!=")),
        value(FilterOp::Gte, tag(">=")),
        value(FilterOp::Lte, tag("<=")),
        value(FilterOp::Gt, tag(">")),
        value(FilterOp::Lt, tag("<")),
        value(FilterOp::Eq, tag("=")),
    ))(input)
}

fn parse_is_null_filter(input: &str) -> IResult<&str, Filter> {
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("IS"))(input)?;
    let (input, negated) = opt(ws(kw("NOT")))(input)?;
    let (input, _) = ws(kw("NULL"))(input)?;
    let op = if negated.is_some() {
        FilterOp::IsNotNull
    } else {
        FilterOp::IsNull
    };
    Ok((
        input,
        Filter {
            field: field.to_string(),
            op,
            value: Literal::Null,
        },
    ))
}

fn parse_contains_filter(input: &str) -> IResult<&str, Filter> {
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("CONTAINS"))(input)?;
    let (input, val) = ws(parse_literal)(input)?;
    Ok((
        input,
        Filter {
            field: field.to_string(),
            op: FilterOp::Contains,
            value: val,
        },
    ))
}

fn parse_comparison_filter(input: &str) -> IResult<&str, Filter> {
    let (input, field) = ws(identifier)(input)?;
    let (input, op) = ws(parse_filter_op)(input)?;
    let (input, val) = ws(parse_literal)(input)?;
    Ok((
        input,
        Filter {
            field: field.to_string(),
            op,
            value: val,
        },
    ))
}

/// `field IN (v1, v2, …)` — scalar-in-set membership. Parenthesised (not the
/// `[...]` list literal) so it never collides with a field whose value is a
/// list. The candidate set is carried as a `Literal::List`, which the engine
/// evaluates with `FilterOp::In` (the mirror of CONTAINS).
fn parse_in_filter(input: &str) -> IResult<&str, Filter> {
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    // Canonical is the bracketed list `IN [a, b]` (same `[]` as xyTalk's list
    // literal, principle 4); `IN (a, b)` stays as a familiar parenthesized alias.
    // Whatever bracket opens must be closed by its pair.
    let (input, open) = ws(alt((char('('), char('['))))(input)?;
    let close = if open == '(' { ')' } else { ']' };
    let (input, first) = ws(parse_literal)(input)?;
    let mut items = vec![first];
    let mut remaining = input;
    loop {
        let trimmed = remaining.trim_start();
        if let Ok((rest, _)) = char::<_, nom::error::Error<&str>>(close)(trimmed) {
            remaining = rest;
            break;
        }
        let (rest, _) = char(',')(trimmed).map_err(|_: nom::Err<nom::error::Error<&str>>| {
            nom::Err::Failure(nom::error::Error::new(trimmed, nom::error::ErrorKind::Char))
        })?;
        let (rest, item) = ws(parse_literal)(rest)?;
        items.push(item);
        remaining = rest;
    }
    Ok((
        remaining,
        Filter {
            field: field.to_string(),
            op: FilterOp::In,
            value: Literal::List(items),
        },
    ))
}

fn parse_filter(input: &str) -> IResult<&str, Filter> {
    alt((
        parse_is_null_filter,
        parse_contains_filter,
        parse_in_filter,
        parse_comparison_filter,
    ))(input)
}

fn parse_where_clause(input: &str) -> IResult<&str, Vec<Filter>> {
    let (input, _) = ws(kw("WHERE"))(input)?;
    separated_list1(ws(kw("AND")), parse_filter)(input)
}

/// V4: Parse WHERE with OR/NOT/AND and parentheses.
/// Precedence: NOT > AND > OR
fn parse_where_expr(input: &str) -> IResult<&str, FilterExpr> {
    let (input, _) = ws(kw("WHERE"))(input)?;
    parse_or_expr(input)
}

fn try_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = input.trim_start();
    let upper = trimmed.get(..keyword.len())?;
    if upper.eq_ignore_ascii_case(keyword) {
        let after = trimmed.get(keyword.len()..)?;
        // Ensure not part of a longer identifier
        if after.is_empty() || !after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            Some(after)
        } else {
            None
        }
    } else {
        None
    }
}

fn parse_or_expr(input: &str) -> IResult<&str, FilterExpr> {
    let (input, first) = parse_and_expr(input)?;
    let mut terms = vec![first];
    let mut remaining = input;
    while let Some(rest) = try_keyword(remaining, "OR") {
        let (rest, next) = parse_and_expr(rest)?;
        terms.push(next);
        remaining = rest;
    }
    if terms.len() == 1 {
        Ok((remaining, terms.into_iter().next().unwrap()))
    } else {
        Ok((remaining, FilterExpr::Or(terms)))
    }
}

fn parse_and_expr(input: &str) -> IResult<&str, FilterExpr> {
    let (input, first) = parse_not_expr(input)?;
    let mut terms = vec![first];
    let mut remaining = input;
    while let Some(rest) = try_keyword(remaining, "AND") {
        let (rest, next) = parse_not_expr(rest)?;
        terms.push(next);
        remaining = rest;
    }
    if terms.len() == 1 {
        Ok((remaining, terms.into_iter().next().unwrap()))
    } else {
        Ok((remaining, FilterExpr::And(terms)))
    }
}

fn parse_not_expr(input: &str) -> IResult<&str, FilterExpr> {
    if let Some(rest) = try_keyword(input, "NOT") {
        let (rest, inner) = parse_not_expr(rest)?;
        return Ok((rest, FilterExpr::Not(Box::new(inner))));
    }
    parse_filter_atom(input)
}

fn parse_filter_atom(input: &str) -> IResult<&str, FilterExpr> {
    let trimmed = input.trim_start();
    // Parenthesized subexpression
    if trimmed.starts_with('(') {
        let (rest, _) = char('(')(trimmed)?;
        let (rest, expr) = parse_or_expr(rest)?;
        let (rest, _) = ws(char(')'))(rest)?;
        return Ok((rest, expr));
    }
    // Simple filter condition
    let (rest, filter) = parse_filter(trimmed)?;
    Ok((rest, FilterExpr::Condition(filter)))
}

fn parse_find_target(input: &str) -> IResult<&str, FindTarget> {
    alt((parse_find_target_lid, parse_find_target_name))(input)
}

fn parse_find_target_lid(input: &str) -> IResult<&str, FindTarget> {
    let (input, _) = ws(kw("LID"))(input)?;
    let (input, _) = char('(')(input)?;
    let (input, s) = quoted_string(input)?;
    let (input, _) = char(')')(input)?;
    Ok((input, FindTarget::ByLid(s)))
}

fn parse_find_target_name(input: &str) -> IResult<&str, FindTarget> {
    let (input, _) = multispace0(input)?;
    alt((
        map(quoted_string, FindTarget::Lobe),
        map(identifier, |s: &str| FindTarget::Lobe(s.to_string())),
    ))(input)
}

// ─── PUT ─────────────────────────────────────────────────────────────────────

fn parse_field_pair(input: &str) -> IResult<&str, PutField> {
    let (input, gravity) = opt(ws(char('*')))(input)?;
    let (input, key) = ws(identifier)(input)?;
    let (input, _) = ws(char(':'))(input)?;
    let (input, val) = ws(parse_literal)(input)?;
    Ok((
        input,
        PutField {
            name: key.to_string(),
            value: val,
            gravity: gravity.is_some(),
        },
    ))
}

fn parse_fields_block(input: &str) -> IResult<&str, Vec<PutField>> {
    let (input, _) = ws(char('{'))(input)?;
    let mut fields = Vec::new();
    let mut remaining = input;
    loop {
        let trimmed = remaining.trim_start();
        if let Ok((rest, _)) = char::<_, nom::error::Error<&str>>('}')(trimmed) {
            return Ok((rest, fields));
        }
        if !fields.is_empty() {
            let (rest, _) = ws(char(','))(trimmed)?;
            remaining = rest;
        } else {
            remaining = trimmed;
        }
        let (rest, field) = parse_field_pair(remaining)?;
        fields.push(field);
        remaining = rest;
    }
}

fn parse_put(input: &str) -> IResult<&str, PutStmt> {
    let (input, _) = ws(kw("PUT"))(input)?;
    let (input, fields) = parse_fields_block(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(alt((quoted_string, map(identifier, String::from))))(input)?;

    // Optional LINK TO ... AS ...
    let (input, link) = opt(parse_put_link_clause)(input)?;

    // Optional ON CONFLICT UPDATE
    let (input, on_conflict) = opt(parse_on_conflict)(input)?;

    Ok((
        input,
        PutStmt {
            fields,
            lobe,
            link,
            on_conflict,
        },
    ))
}

// ─── PUT BATCH ───────────────────────────────────────────────────────────────

fn parse_put_batch(input: &str) -> IResult<&str, PutBatchStmt> {
    let (input, _) = ws(kw("PUT"))(input)?;
    let (input, _) = ws(kw("BATCH"))(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(alt((quoted_string, map(identifier, String::from))))(input)?;
    let (input, records) = delimited(
        ws(char('[')),
        separated_list1(ws(char(',')), parse_fields_block),
        ws(char(']')),
    )(input)?;
    let (input, link) = opt(parse_put_link_clause)(input)?;
    let (input, on_conflict) = opt(parse_on_conflict)(input)?;

    Ok((
        input,
        PutBatchStmt {
            records,
            lobe,
            link,
            on_conflict,
        },
    ))
}

fn parse_put_link_clause(input: &str) -> IResult<&str, LinkClause> {
    let (input, _) = ws(kw("LINK"))(input)?;
    let (input, _) = ws(kw("TO"))(input)?;
    let (input, target) = parse_find_target(input)?;
    let (input, filters) = opt(parse_where_clause)(input)?;

    let (input, _) = ws(kw("AS"))(input)?;
    let (input, rel_name) = ws(quoted_string)(input)?;

    Ok((
        input,
        LinkClause {
            target,
            filters: filters.unwrap_or_default(),
            relation_name: rel_name,
        },
    ))
}

fn parse_on_conflict(input: &str) -> IResult<&str, OnConflict> {
    let (input, _) = ws(kw("ON"))(input)?;
    let (input, _) = ws(kw("CONFLICT"))(input)?;
    let (input, _) = ws(kw("UPDATE"))(input)?;
    Ok((input, OnConflict::Update))
}

// ─── FIND ────────────────────────────────────────────────────────────────────

fn parse_find(input: &str) -> IResult<&str, FindStmt> {
    let (input, _) = ws(kw("FIND"))(input)?;
    let (input, target) = ws(parse_find_target)(input)?;
    let (input, filters) = opt(parse_where_clause)(input)?;
    let (input, limit) = opt(parse_limit)(input)?;
    let (input, cursor) = opt(parse_cursor)(input)?;
    let (input, _) = multispace0(input)?;

    Ok((
        input,
        FindStmt {
            target,
            filters: filters.unwrap_or_default(),
            limit,
            cursor,
        },
    ))
}

// ─── PULL ────────────────────────────────────────────────────────────────────

fn parse_pull_params(input: &str) -> IResult<&str, (u32, Option<String>)> {
    let mut depth = 1u32;
    let mut only = None;
    let mut rest = input;

    loop {
        let (r, _) = multispace0(rest)?;
        if let Ok((r2, _)) = kw("depth")(r) {
            let (r2, _) = ws(char('='))(r2)?;
            let (r2, d) = take_while1(|c: char| c.is_ascii_digit())(r2)?;
            depth = d.parse().unwrap_or(1);
            rest = r2;
        } else if let Ok((r2, _)) = kw("only")(r) {
            let (r2, _) = ws(char('='))(r2)?;
            let (r2, name) = identifier(r2)?;
            only = Some(name.to_string());
            rest = r2;
        } else {
            break;
        }
    }

    Ok((rest, (depth, only)))
}

fn parse_pull_step(input: &str) -> IResult<&str, PullStmt> {
    let (input, _) = ws(kw("PULL"))(input)?;
    let (input, (depth, only)) = parse_pull_params(input)?;
    Ok((
        input,
        PullStmt {
            target: None,
            depth,
            only,
        },
    ))
}

fn parse_pull_full(input: &str) -> IResult<&str, PullStmt> {
    let (input, _) = ws(kw("PULL"))(input)?;
    let (input, _) = ws(kw("FROM"))(input)?;
    let (input, target) = ws(parse_find_target)(input)?;
    let (input, (depth, only)) = parse_pull_params(input)?;
    Ok((
        input,
        PullStmt {
            target: Some(target),
            depth,
            only,
        },
    ))
}

// ─── SCAN ────────────────────────────────────────────────────────────────────

fn parse_scan(input: &str) -> IResult<&str, ScanStmt> {
    let (input, _) = ws(kw("SCAN"))(input)?;
    let (input, lobe) = ws(alt((quoted_string, map(identifier, String::from))))(input)?;
    let (input, filter_expr) = opt(parse_where_expr)(input)?;
    let (input, order_by) = opt(parse_order_by)(input)?;
    let (input, limit) = opt(parse_limit)(input)?;
    let (input, cursor) = opt(parse_cursor)(input)?;
    Ok((
        input,
        ScanStmt {
            lobe,
            filter_expr,
            order_by,
            limit,
            cursor,
        },
    ))
}

/// v0.2.5.1: `CURSOR "<opaque-token>"` — opaque pagination token from a previous SCAN.
/// The parser carries the raw string; encode/decode and validation happen in the engine.
fn parse_cursor(input: &str) -> IResult<&str, String> {
    let (input, _) = ws(kw("CURSOR"))(input)?;
    let (input, token) = ws(quoted_string)(input)?;
    Ok((input, token))
}

fn parse_order_by(input: &str) -> IResult<&str, OrderBy> {
    let (input, _) = ws(kw("ORDER"))(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, desc) = opt(alt((
        value(true, ws(kw("DESC"))),
        value(false, ws(kw("ASC"))),
    )))(input)?;
    Ok((
        input,
        OrderBy {
            field: field.to_string(),
            descending: desc.unwrap_or(false),
        },
    ))
}

fn parse_limit(input: &str) -> IResult<&str, u64> {
    let (input, _) = ws(kw("LIMIT"))(input)?;
    let (input, n) = ws(take_while1(|c: char| c.is_ascii_digit()))(input)?;
    let val: u64 = n.parse().map_err(|_| {
        nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Digit))
    })?;
    Ok((input, val))
}

// ─── SET ─────────────────────────────────────────────────────────────────────

fn parse_assignment(input: &str) -> IResult<&str, (String, Literal)> {
    let (input, key) = ws(identifier)(input)?;
    let (input, _) = ws(char('='))(input)?;
    let (input, val) = ws(parse_literal)(input)?;
    Ok((input, (key.to_string(), val)))
}

fn parse_set_step(input: &str) -> IResult<&str, SetStmt> {
    let (input, _) = ws(kw("SET"))(input)?;
    let (input, assignments) = separated_list1(ws(char(',')), parse_assignment)(input)?;
    Ok((
        input,
        SetStmt {
            target: None,
            assignments,
            filter_expr: None,
        },
    ))
}

fn parse_set_full(input: &str) -> IResult<&str, SetStmt> {
    let (input, _) = ws(kw("SET"))(input)?;
    let (input, target) = ws(parse_find_target)(input)?;
    let (input, assignments) = separated_list1(ws(char(',')), parse_assignment)(input)?;
    // Optional `WHERE <expr>` after the assignment list (greedy `separated_list1`
    // means WHERE follows it). Full OR/NOT/IN tree (xyTalk v1 P1).
    let (input, filter_expr) = opt(parse_where_expr)(input)?;
    Ok((
        input,
        SetStmt {
            target: Some(target),
            assignments,
            filter_expr,
        },
    ))
}

// ─── DELETE ──────────────────────────────────────────────────────────────────

fn parse_delete_step(input: &str) -> IResult<&str, DeleteStmt> {
    let (input, _) = ws(kw("DELETE"))(input)?;
    Ok((
        input,
        DeleteStmt {
            target: None,
            filter_expr: None,
        },
    ))
}

fn parse_delete_full(input: &str) -> IResult<&str, DeleteStmt> {
    let (input, _) = ws(kw("DELETE"))(input)?;
    let (input, target) = ws(parse_find_target)(input)?;
    // The require-WHERE check (P7) lives in the dispatch so it can return a
    // teaching error; here WHERE stays optional.
    let (input, filter_expr) = opt(parse_where_expr)(input)?;
    Ok((
        input,
        DeleteStmt {
            target: Some(target),
            filter_expr,
        },
    ))
}

/// `PURGE "lobe"` — empty a whole lobe (the explicit form of total deletion).
fn parse_purge(input: &str) -> IResult<&str, PurgeStmt> {
    let (input, _) = ws(kw("PURGE"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, PurgeStmt { lobe }))
}

/// `FETCH "a", "b", "c" WHERE <expr> [AS {n1, n2, n3}]` — multi-lobe co-located
/// read. WHERE and AS validity (present / one-name-per-lobe) is checked by the
/// dispatch so it can teach; here both parse as optional.
fn parse_fetch(input: &str) -> IResult<&str, FetchStmt> {
    let (input, _) = ws(kw("FETCH"))(input)?;
    let (input, lobes) = separated_list1(ws(char(',')), ws(quoted_string))(input)?;
    let (input, filter_expr) = opt(parse_where_expr)(input)?;
    let (input, names) = opt(parse_fetch_as)(input)?;
    Ok((
        input,
        FetchStmt {
            lobes,
            filter_expr,
            names,
        },
    ))
}

/// `AS {n1, n2, n3}` — positional section names (braces mirror PUT/SHAPE).
fn parse_fetch_as(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = ws(kw("AS"))(input)?;
    let (input, _) = ws(char('{'))(input)?;
    let (input, names) = separated_list1(ws(char(',')), ws(identifier))(input)?;
    let (input, _) = ws(char('}'))(input)?;
    Ok((input, names.into_iter().map(|s| s.to_string()).collect()))
}

// ─── LINK ────────────────────────────────────────────────────────────────────

fn parse_link(input: &str) -> IResult<&str, LinkStmt> {
    let (input, _) = ws(kw("LINK"))(input)?;
    let (input, source) = ws(parse_find_target)(input)?;
    // Optional `WHERE <expr>` on the source side. Full OR/NOT/IN tree (v1 P1).
    let (input, source_filter_expr) = opt(parse_where_expr)(input)?;
    let (input, _) = ws(kw("TO"))(input)?;
    let (input, target) = ws(parse_find_target)(input)?;
    // Optional `WHERE <expr>` on the target side.
    let (input, target_filter_expr) = opt(parse_where_expr)(input)?;
    let (input, _) = ws(kw("AS"))(input)?;
    let (input, rel_name) = ws(quoted_string)(input)?;
    Ok((
        input,
        LinkStmt {
            source,
            target,
            relation_name: rel_name,
            source_filter_expr,
            target_filter_expr,
        },
    ))
}

// ─── ANCHOR ──────────────────────────────────────────────────────────────────

fn parse_anchor(input: &str) -> IResult<&str, AnchorStmt> {
    let (input, _) = ws(kw("ANCHOR"))(input)?;
    let (input, field) = ws(quoted_string)(input)?;
    let (input, _) = ws(kw("UNIQUE"))(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, AnchorStmt { field, lobe }))
}

/// `VECTOR <field> IN "<lobe>"` — declare the lobe's searchable embedding field.
/// No `BY`: unlike `GRAVITY BY`, there is no transform — it is the field itself
/// (mirrors the bare-field form of `PIN <field> IN`).
fn parse_vector(input: &str) -> IResult<&str, VectorStmt> {
    let (input, _) = ws(kw("VECTOR"))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((
        input,
        VectorStmt {
            field: field.to_string(),
            lobe,
        },
    ))
}

// ─── SATELLITE BY ────────────────────────────────────────────────────────────

/// `SATELLITE BY <field> IN "<lobe>"` — a single field, `BY` like gravity but a
/// bare identifier (no transform/composite) like vector.
fn parse_satellite(input: &str) -> IResult<&str, SatelliteStmt> {
    let (input, _) = ws(kw("SATELLITE"))(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((
        input,
        SatelliteStmt {
            field: field.to_string(),
            lobe,
        },
    ))
}

// ─── GRAVITY BY ──────────────────────────────────────────────────────────────

fn parse_gravity(input: &str) -> IResult<&str, GravityStmt> {
    let (input, _) = ws(kw("GRAVITY"))(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, spec) = parse_gravity_expr(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, GravityStmt { lobe, spec }))
}

/// `<expr>` = `(a, b, …)` | `lower(field)` | `trim(field)` | `field`.
/// Composite and transform are tried before the bare-field form; nom
/// backtracks, so a field literally named `lower`/`trim` (no parens) still
/// parses as `Raw`.
fn parse_gravity_expr(input: &str) -> IResult<&str, GravitySpecAst> {
    alt((
        parse_gravity_composite,
        parse_gravity_transform,
        map(ws(identifier), |f: &str| GravitySpecAst::Raw(f.to_string())),
    ))(input)
}

fn parse_gravity_composite(input: &str) -> IResult<&str, GravitySpecAst> {
    let (input, _) = ws(char('('))(input)?;
    let (input, fields) = separated_list1(ws(char(',')), ws(identifier))(input)?;
    let (input, _) = ws(char(')'))(input)?;
    Ok((
        input,
        GravitySpecAst::Composite(fields.into_iter().map(String::from).collect()),
    ))
}

fn parse_gravity_transform(input: &str) -> IResult<&str, GravitySpecAst> {
    let (input, name) = ws(alt((kw("lower"), kw("trim"))))(input)?;
    let (input, _) = ws(char('('))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(char(')'))(input)?;
    let transform = if name.eq_ignore_ascii_case("lower") {
        GravityTransform::Lower
    } else {
        GravityTransform::Trim
    };
    Ok((
        input,
        GravitySpecAst::Normalized(field.to_string(), transform),
    ))
}

// ─── LOBE ────────────────────────────────────────────────────────────────────

fn parse_lobe(input: &str) -> IResult<&str, LobeStmt> {
    let (input, _) = ws(kw("LOBE"))(input)?;
    let (input, name) = ws(quoted_string)(input)?;
    let (input, hint) = opt(parse_lobe_hint)(input)?;
    Ok((input, LobeStmt { name, hint }))
}

fn parse_lobe_hint(input: &str) -> IResult<&str, String> {
    let (input, _) = ws(kw("HINT"))(input)?;
    let (input, _) = ws(char('='))(input)?;
    ws(quoted_string)(input)
}

// ─── INCACHE / OUTCACHE (V5 RecordCache control) ────────────────────────────────

/// `INCACHE <lobe> [WHERE <v4_expr>]` — load matching records into RecordCache.
/// Lobe accepts both quoted and unquoted forms (matches PUT/SCAN convention).
fn parse_incache(input: &str) -> IResult<&str, InCacheStmt> {
    let (input, _) = ws(kw("INCACHE"))(input)?;
    let (input, lobe) = ws(alt((quoted_string, map(identifier, String::from))))(input)?;
    let (input, filter_expr) = opt(parse_where_expr)(input)?;
    Ok((input, InCacheStmt { lobe, filter_expr }))
}

/// `OUTCACHE <lobe>` — evict the lobe from RecordCache. The pre-v0.2.5.1
/// hand-rolled parser produced `OutCache("")` silently for the bare
/// `OUTCACHE` keyword; nom rejects with a clear error instead.
fn parse_outcache(input: &str) -> IResult<&str, String> {
    let (input, _) = ws(kw("OUTCACHE"))(input)?;
    let (input, lobe) = ws(alt((quoted_string, map(identifier, String::from))))(input)?;
    Ok((input, lobe))
}

// ─── SHOW ────────────────────────────────────────────────────────────────────

fn parse_show(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("SHOW"))(input)?;
    alt((
        parse_show_scan_stats,
        parse_show_profile,
        parse_show_cache,
        parse_show_throttle,
        parse_show_ghosts,
        parse_show_anchors,
        parse_show_lobes,
    ))(input)
}

fn parse_show_cache(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("CACHE"))(input)?;
    Ok((input, ShowStmt::Cache))
}

fn parse_show_lobes(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("LOBES"))(input)?;
    Ok((input, ShowStmt::Lobes))
}

fn parse_show_anchors(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("ANCHORS"))(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, ShowStmt::Anchors(lobe)))
}

fn parse_show_throttle(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("THROTTLE"))(input)?;
    Ok((input, ShowStmt::Throttle))
}

fn parse_show_ghosts(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("GHOSTS"))(input)?;
    Ok((input, ShowStmt::Ghosts))
}

fn parse_show_scan_stats(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("SCAN"))(input)?;
    let (input, _) = ws(kw("STATS"))(input)?;
    Ok((input, ShowStmt::ScanStats))
}

fn parse_show_profile(input: &str) -> IResult<&str, ShowStmt> {
    let (input, _) = ws(kw("PROFILE"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, ShowStmt::Profile(lobe)))
}

// ─── PIN / UNPIN ────────────────────────────────────────────────────────────

// PIN campo1, campo2 IN "lobe"
fn parse_pin(input: &str) -> IResult<&str, PinStmt> {
    let (input, _) = ws(kw("PIN"))(input)?;
    let (input, fields) =
        separated_list1(ws(char(',')), map(ws(identifier), |s: &str| s.to_string()))(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, PinStmt { fields, lobe }))
}

// UNPIN campo1 [, campo2] IN "lobe"
fn parse_unpin(input: &str) -> IResult<&str, UnpinStmt> {
    let (input, _) = ws(kw("UNPIN"))(input)?;
    let (input, fields) =
        separated_list1(ws(char(',')), map(ws(identifier), |s: &str| s.to_string()))(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, UnpinStmt { fields, lobe }))
}

// ─── GHOST LOBES ────────────────────────────────────────────────────────────

/// The shape of a ghost past `FROM "lobe" [WHERE …]`: the order, the optional
/// grouping/aggregation, and the optional projection. Both the canonical
/// pipeline form and the classic clause form fill it identically.
struct GhostShape {
    order_by: String,
    order_metric: Option<TopBy>,
    sort_descending: bool,
    group_by: Vec<String>,
    aggregates: Vec<Aggregate>,
    embed: Vec<String>,
}

/// A ghost's ORDER target: a declared aggregate metric (`sum(monto)`, tried
/// first — it has parens an identifier can't match) or a record field. A metric
/// target keeps the grouped rollup ordered by that metric (O(N) TOP); a field
/// target is the classic covering-entry order.
fn parse_ghost_order_target(input: &str) -> IResult<&str, (String, Option<TopBy>)> {
    alt((
        map(parse_aggregate_func, |f| {
            (String::new(), Some(TopBy::Metric(f)))
        }),
        map(ws(identifier), |s: &str| (s.to_string(), None)),
    ))(input)
}

fn parse_ghost_direction(input: &str) -> IResult<&str, bool> {
    let (input, desc) = opt(alt((
        map(ws(kw("DESC")), |_| true),
        map(ws(kw("ASC")), |_| false),
    )))(input)?;
    Ok((input, desc.unwrap_or(false)))
}

/// CREATE GHOST "name" FROM "lobe" [WHERE …]
///   canonical: `[| GROUP BY …] [| AGGREGATE …] | TAKE BY <target> [DESC|ASC] [| EMBED …]`
///   alias:     `ORDER BY <target> [DESC|ASC] [GROUP BY …] [AGGREGATE …] [EMBED …]`
///
/// A ghost is a saved query: the pipeline form reads exactly like the SCAN it
/// serves, with `TAKE BY <metric>` (no `n`) declaring "keep this ordered so
/// `TAKE n BY <metric>` is O(N)". The classic clause form stays as an alias.
fn parse_create_ghost(input: &str) -> IResult<&str, CreateGhostStmt> {
    let (input, _) = ws(kw("CREATE"))(input)?;
    let (input, _) = ws(kw("GHOST"))(input)?;
    let (input, name) = ws(quoted_string)(input)?;
    let (input, _) = ws(kw("FROM"))(input)?;
    let (input, source_lobe) = ws(quoted_string)(input)?;
    // Full OR/NOT/In expression, not just flat-AND: a ghost can now carry any
    // membership predicate the engine evaluates with one filter walker.
    let (input, filter) = opt(parse_where_expr)(input)?;
    // Pipeline form is canonical (leads with `|`); the ORDER BY clause is alias.
    let (input, shape) = alt((parse_ghost_pipeline_shape, parse_ghost_clause_shape))(input)?;
    Ok((
        input,
        CreateGhostStmt {
            name,
            source_lobe,
            // No WHERE → And([]) (covers the whole lobe), matching the old
            // empty-Vec<Filter> semantics under the one filter walker.
            filter: filter.unwrap_or_else(|| FilterExpr::And(Vec::new())),
            order_by: shape.order_by,
            sort_descending: shape.sort_descending,
            order_metric: shape.order_metric,
            group_by: shape.group_by,
            aggregates: shape.aggregates,
            embed: shape.embed,
        },
    ))
}

/// Canonical: `[| GROUP BY …] [| AGGREGATE …] | TAKE BY <target> [DESC|ASC] [| EMBED …]`.
/// `| TAKE BY` is required — it's the order declaration (the pipeline peer of the
/// mandatory `ORDER BY`), placed last to match how a SCAN query reads.
fn parse_ghost_pipeline_shape(input: &str) -> IResult<&str, GhostShape> {
    let pipe = |i| ws(char('|'))(i);
    let (input, group_by) = opt(preceded(pipe, parse_group_by))(input)?;
    let (input, aggregates) = opt(preceded(pipe, parse_aggregate))(input)?;
    let (input, _) = pipe(input)?;
    let (input, _) = ws(kw("TAKE"))(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, (order_by, order_metric)) = parse_ghost_order_target(input)?;
    let (input, sort_descending) = parse_ghost_direction(input)?;
    let (input, embed) = opt(preceded(pipe, parse_ghost_embed))(input)?;
    Ok((
        input,
        GhostShape {
            order_by,
            order_metric,
            sort_descending,
            group_by: group_by.unwrap_or_default(),
            aggregates: aggregates.unwrap_or_default(),
            embed: embed.unwrap_or_default(),
        },
    ))
}

/// Alias: `ORDER BY <target> [DESC|ASC] [GROUP BY …] [AGGREGATE …] [EMBED …]`.
fn parse_ghost_clause_shape(input: &str) -> IResult<&str, GhostShape> {
    let (input, _) = ws(kw("ORDER"))(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, (order_by, order_metric)) = parse_ghost_order_target(input)?;
    let (input, sort_descending) = parse_ghost_direction(input)?;
    let (input, group_by) = opt(parse_group_by)(input)?;
    let (input, aggregates) = opt(parse_aggregate)(input)?;
    let (input, embed) = opt(parse_ghost_embed)(input)?;
    Ok((
        input,
        GhostShape {
            order_by,
            order_metric,
            sort_descending,
            group_by: group_by.unwrap_or_default(),
            aggregates: aggregates.unwrap_or_default(),
            embed: embed.unwrap_or_default(),
        },
    ))
}

/// PROJECT field1, field2, ... — fields to embed in ghost entries.
fn parse_ghost_embed(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = ws(kw("EMBED"))(input)?;
    let (input, first) = ws(identifier)(input)?;
    let mut fields = vec![first.to_string()];
    let mut rest = input;
    loop {
        let comma: std::result::Result<(&str, char), nom::Err<nom::error::Error<&str>>> =
            ws(nom::character::complete::char(','))(rest);
        if let Ok((r, _)) = comma {
            let field_res: std::result::Result<(&str, &str), nom::Err<nom::error::Error<&str>>> =
                ws(identifier)(r);
            if let Ok((r2, field)) = field_res {
                fields.push(field.to_string());
                rest = r2;
                continue;
            }
        }
        break;
    }
    Ok((rest, fields))
}

// SCAN GHOST "name" [WHERE filters]
fn parse_scan_ghost(input: &str) -> IResult<&str, ScanGhostStmt> {
    let (input, _) = ws(kw("SCAN"))(input)?;
    let (input, _) = ws(kw("GHOST"))(input)?;
    let (input, name) = ws(quoted_string)(input)?;
    // Full OR/NOT/IN tree (xyTalk v1 P1); execution pushes AND-pure into
    // read_topn and scans + walker-filters for OR/NOT.
    let (input, filter_expr) = opt(parse_where_expr)(input)?;
    let (input, limit) = opt(parse_limit)(input)?;
    Ok((
        input,
        ScanGhostStmt {
            name,
            filter_expr,
            limit,
        },
    ))
}

// REFRESH GHOST "name"
fn parse_refresh_ghost(input: &str) -> IResult<&str, String> {
    let (input, _) = ws(kw("REFRESH"))(input)?;
    let (input, _) = ws(kw("GHOST"))(input)?;
    let (input, name) = ws(quoted_string)(input)?;
    Ok((input, name))
}

// DROP GHOST "name"
fn parse_drop_ghost(input: &str) -> IResult<&str, String> {
    let (input, _) = ws(kw("DROP"))(input)?;
    let (input, _) = ws(kw("GHOST"))(input)?;
    let (input, name) = ws(quoted_string)(input)?;
    Ok((input, name))
}

// ─── FOLLOW ──────────────────────────────────────────────────────────────

/// `FOLLOW <field> TO "<lobe>" ON <target_field>` — cross-entity expansion.
fn parse_follow(input: &str) -> IResult<&str, FollowStmt> {
    let (input, _) = ws(kw("FOLLOW"))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("TO"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    let (input, _) = ws(kw("ON"))(input)?;
    let (input, target_field) = ws(identifier)(input)?;
    Ok((
        input,
        FollowStmt {
            field: field.to_string(),
            lobe,
            target_field: target_field.to_string(),
        },
    ))
}

// ─── NEAREST ──────────────────────────────────────────────────────────────

/// The query side: a bound `$param`, a `REF "id"`, or an inline list literal.
/// The engine never embeds text — the caller supplies the query vector.
fn parse_nearest_query(input: &str) -> IResult<&str, NearestQuery> {
    alt((
        parse_nearest_param,
        parse_nearest_ref,
        map(parse_literal, NearestQuery::Vector),
    ))(input)
}

fn parse_nearest_param(input: &str) -> IResult<&str, NearestQuery> {
    let (input, _) = char('$')(input)?;
    let (input, name) = identifier(input)?;
    Ok((input, NearestQuery::Param(name.to_string())))
}

/// Parse a bound-parameter placeholder in literal position: `$name` or `$1`.
/// The name (identifier or positional digits) is captured without the `$`.
fn parse_param_literal(input: &str) -> IResult<&str, Literal> {
    let (input, _) = char('$')(input)?;
    let (input, name) = alt((identifier, take_while1(|c: char| c.is_ascii_digit())))(input)?;
    Ok((input, Literal::Param(name.to_string())))
}

fn parse_nearest_ref(input: &str) -> IResult<&str, NearestQuery> {
    let (input, _) = ws(kw("REF"))(input)?;
    let (input, id) = ws(quoted_string)(input)?;
    Ok((input, NearestQuery::Ref(id)))
}

/// A bare unsigned integer (a `k`, a limit).
fn parse_u64(input: &str) -> IResult<&str, u64> {
    let (input, digits) = take_while1(|c: char| c.is_ascii_digit())(input)?;
    let n: u64 = digits.parse().map_err(|_| {
        nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Digit))
    })?;
    Ok((input, n))
}

/// `NEAREST k BY <field> TO <query> [USING <metric>]` — canonical phrase form;
/// `NEAREST(<field>, <query>, k, <metric>)` — function alias. Both keep the `k`
/// records whose `field` embedding is closest to `query`. `<query>` is a bound
/// `$param`, an inline `[vector…]`, or `REF "id"` ("more like this"). `<metric>`
/// is the raw name (`cosine`/`dot`/`l2`), defaulting to `cosine` in the phrase
/// form when `USING` is omitted.
fn parse_nearest(input: &str) -> IResult<&str, NearestStmt> {
    let (input, _) = ws(kw("NEAREST"))(input)?;
    alt((parse_nearest_func, parse_nearest_phrase))(input)
}

/// Function alias: `(<field>, <query>, k, <metric>)`.
fn parse_nearest_func(input: &str) -> IResult<&str, NearestStmt> {
    let (input, _) = ws(char('('))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(char(','))(input)?;
    let (input, query) = ws(parse_nearest_query)(input)?;
    let (input, _) = ws(char(','))(input)?;
    let (input, k) = ws(parse_u64)(input)?;
    let (input, _) = ws(char(','))(input)?;
    let (input, metric) = ws(identifier)(input)?;
    let (input, _) = ws(char(')'))(input)?;
    Ok((
        input,
        NearestStmt {
            field: field.to_string(),
            query,
            k,
            metric: metric.to_string(),
        },
    ))
}

/// Canonical phrase: `k BY <field> TO <query> [USING <metric>]`.
fn parse_nearest_phrase(input: &str) -> IResult<&str, NearestStmt> {
    let (input, k) = ws(parse_u64)(input)?;
    let (input, _) = ws(kw("BY"))(input)?;
    let (input, field) = ws(identifier)(input)?;
    let (input, _) = ws(kw("TO"))(input)?;
    let (input, query) = ws(parse_nearest_query)(input)?;
    let (input, metric) = opt(preceded(ws(kw("USING")), ws(identifier)))(input)?;
    Ok((
        input,
        NearestStmt {
            field: field.to_string(),
            query,
            k,
            metric: metric.unwrap_or("cosine").to_string(),
        },
    ))
}

// ─── AGGREGATE ──────────────────────────────────────────────────────────────

fn parse_aggregate(input: &str) -> IResult<&str, Vec<Aggregate>> {
    let (input, _) = ws(kw("AGGREGATE"))(input)?;
    separated_list1(ws(char(',')), parse_aggregate_item)(input)
}

/// One metric: `func() [AS <alias>] [WHERE <expr>]`. The alias precedes the
/// per-metric WHERE (mirrors the closed Q8 syntax). Both are optional, so a
/// bare `func()` parses to `Aggregate { func, filter: None, alias: None }`.
fn parse_aggregate_item(input: &str) -> IResult<&str, Aggregate> {
    let (input, func) = parse_aggregate_func(input)?;
    let (input, alias) = opt(preceded(ws(kw("AS")), ws(identifier)))(input)?;
    let (input, filter) = opt(parse_where_expr)(input)?;
    Ok((
        input,
        Aggregate {
            func,
            filter,
            alias: alias.map(|s| s.to_string()),
        },
    ))
}

fn parse_aggregate_func(input: &str) -> IResult<&str, AggregateFunc> {
    alt((
        parse_agg_count,
        parse_agg_sum,
        parse_agg_avg,
        parse_agg_min,
        parse_agg_max,
    ))(input)
}

fn parse_agg_count(input: &str) -> IResult<&str, AggregateFunc> {
    let (input, _) = ws(kw("count"))(input)?;
    let (input, _) = char('(')(input)?;
    // `count(*)` accepted as an alias of `count()`; both mean "count rows in
    // the group". No `count(field)` — that stays unsupported.
    let (input, _) = opt(ws(char('*')))(input)?;
    let (input, _) = ws(char(')'))(input)?;
    Ok((input, AggregateFunc::Count))
}

fn parse_agg_field_func<'a>(name: &'static str) -> impl FnMut(&'a str) -> IResult<&'a str, String> {
    move |input: &'a str| {
        let (input, _) = ws(kw(name))(input)?;
        let (input, _) = char('(')(input)?;
        let (input, field) = ws(identifier)(input)?;
        let (input, _) = char(')')(input)?;
        Ok((input, field.to_string()))
    }
}

fn parse_agg_sum(input: &str) -> IResult<&str, AggregateFunc> {
    let (input, field) = parse_agg_field_func("sum")(input)?;
    Ok((input, AggregateFunc::Sum(field)))
}

fn parse_agg_avg(input: &str) -> IResult<&str, AggregateFunc> {
    let (input, field) = parse_agg_field_func("avg")(input)?;
    Ok((input, AggregateFunc::Avg(field)))
}

fn parse_agg_min(input: &str) -> IResult<&str, AggregateFunc> {
    let (input, field) = parse_agg_field_func("min")(input)?;
    Ok((input, AggregateFunc::Min(field)))
}

fn parse_agg_max(input: &str) -> IResult<&str, AggregateFunc> {
    let (input, field) = parse_agg_field_func("max")(input)?;
    Ok((input, AggregateFunc::Max(field)))
}

// ─── ANALYZE ────────────────────────────────────────────────────────────────

fn parse_analyze(input: &str) -> IResult<&str, String> {
    let (input, _) = ws(kw("ANALYZE"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, lobe))
}

// ─── AUTOANCHOR APPLY ───────────────────────────────────────────────────────

fn parse_autoanchor_apply(input: &str) -> IResult<&str, AutoAnchorApplyStmt> {
    let (input, _) = ws(kw("AUTOANCHOR"))(input)?;
    let (input, _) = ws(kw("APPLY"))(input)?;
    let (input, field) = ws(quoted_string)(input)?;
    let (input, _) = ws(kw("IN"))(input)?;
    let (input, lobe) = ws(quoted_string)(input)?;
    Ok((input, AutoAnchorApplyStmt { field, lobe }))
}
