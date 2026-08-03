use super::*;

impl Engine {
    /// Execute a parsed xyTalk statement.
    pub fn execute(&self, stmt: Statement) -> Result<QueryResult> {
        // D1 guard: while the database holds pre-D1 (name+value) gravity data,
        // refuse the data ops that read or write gravity buckets — they would
        // silently miss or misplace records against the value-only fast path.
        // Admin/DDL (incl. `migrate`) stays open so the operator can rehash.
        if self
            .gravity_needs_migration
            .load(std::sync::atomic::Ordering::Relaxed)
            && stmt_touches_gravity_data(&stmt)
        {
            return Err(XyzError::InvalidQuery(
                "database holds pre-0.8 (name+value) gravity data; run `migrate` before \
                 reads/writes — D1 rehashes gravity keys to the value-only convention"
                    .into(),
            ));
        }
        match stmt {
            Statement::Put(s) => crate::ops::put::execute_put(self, s),
            Statement::PutBatch(s) => crate::ops::put::execute_put_batch(self, s),
            Statement::Find(s) => crate::ops::find::execute_find(self, s),
            Statement::Pull(s) => crate::ops::pull::execute_pull(self, s, None),
            Statement::Scan(s) => {
                let scan_result = crate::ops::scan::execute_scan(self, s)?;
                Ok(scan_result.query_result)
            }
            Statement::Set(s) => crate::ops::set::execute_set(self, s, None),
            Statement::Delete(s) => crate::ops::delete::execute_delete(self, s, None),
            Statement::Purge(s) => crate::ops::delete::execute_purge(self, s),
            Statement::Fetch(s) => crate::ops::fetch::execute_fetch(self, s),
            Statement::Link(s) => crate::ops::link::execute_link(self, s),
            Statement::Anchor(s) => self.execute_anchor(s),
            Statement::Gravity(s) => self.execute_gravity(s),
            Statement::Vector(s) => self.execute_vector(s),
            Statement::Satellite(s) => self.execute_satellite(s),
            Statement::Lobe(s) => self.execute_lobe(s),
            Statement::Show(s) => self.execute_show(s),
            Statement::AutoAnchorApply(s) => self.execute_autoanchor_apply(s),
            Statement::CreateGhost(s) => self.execute_create_ghost(s),
            Statement::ScanGhost(s) => self.execute_scan_ghost(s),
            Statement::RefreshGhost(name) => self.execute_refresh_ghost(&name),
            Statement::DropGhost(name) => self.execute_drop_ghost(&name),
            Statement::Analyze(lobe) => {
                warn_admin_statement_deprecated("ANALYZE", "analyze");
                crate::analyze::execute_analyze(self, &lobe)
            }
            Statement::Compact => {
                warn_admin_statement_deprecated("COMPACT", "compact");
                self.execute_compact()
            }
            Statement::Scrub => self.execute_scrub(),
            Statement::BulkMode(on) => {
                warn_admin_statement_deprecated("BULKMODE", "bulkmode");
                self.turba.set_compaction_enabled(!on);
                // v0.6.2 §12.10 — gate BatchIngest heat recording off for
                // the duration of the declared bulk load (see is_bulk_loading).
                self.bulk_loading
                    .store(on, std::sync::atomic::Ordering::Relaxed);
                // 0.7.6: defer ghost aggregate maintenance during bulk —
                // a lightweight ghost would otherwise pay a per-record
                // disk RMW (observed collapsing the scale-1 load to ~tens
                // of records/s). The bulk contract already requires
                // REFRESH after the load, which rebuilds aggregates.
                self.ghost_manager.set_bulk_mode(on);
                let msg = if on {
                    "BULKMODE ON: auto-compaction disabled"
                } else {
                    "BULKMODE OFF: auto-compaction enabled"
                };
                tracing::info!("{msg}");
                Ok(QueryResult::Ok {
                    lid: None,
                    message: msg.to_string(),
                })
            }
            Statement::Migrate(lobe) => {
                warn_admin_statement_deprecated("MIGRATE", "migrate");
                self.execute_migrate(lobe)
            }
            Statement::InCache(s) => self.execute_incache(s),
            Statement::OutCache(lobe) => self.execute_outcache(&lobe),
            Statement::Pin(s) => self.execute_pin(s),
            Statement::Unpin(s) => self.execute_unpin(s),
            Statement::Pipeline(steps) => crate::planner::execute_pipeline(self, steps),
        }
    }

    /// Execute a raw xyTalk string (parse + execute).
    /// Supports multiple statements separated by `;` — CREATE GHOST on the
    /// same lobe are auto-batched into a single scan.
    pub fn run(&self, input: &str) -> Result<QueryResult> {
        // A `$param` with no bindings is an unbound parameter: run_with_params
        // with an empty map rejects it (never treats `$x` as a literal).
        self.run_with_params(input, &HashMap::new())
    }

    /// As [`run`](Self::run), but with bound parameters. Every `$name`
    /// placeholder in the statement — a `WHERE`/`PUT`/`SET` value, a list/map
    /// element, or a `NEAREST` query vector — is replaced by `params[name]`
    /// before execution. The value travels out-of-band, so untrusted text never
    /// enters the query string as syntax: this is the anti-injection guarantee.
    ///
    /// # Arguments
    /// * `input` - one or more xyTalk statements.
    /// * `params` - bound values keyed by parameter name (without the `$`).
    ///
    /// # Errors
    /// [`XyzError::InvalidQuery`] if a referenced parameter is unbound, is an
    /// unsupported type for binding, or (for `NEAREST`) is not a numeric vector;
    /// plus anything execution can return.
    pub fn run_with_params(
        &self,
        input: &str,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<QueryResult> {
        // The bind walk only matters when a `$` placeholder is present; skip it
        // for the common param-free path (incl. hot vector ingest — all digits).
        let needs_bind = input.contains('$');
        if !input.contains(';') {
            let mut stmt = xytalk_parser::parse(input)?;
            if needs_bind {
                Self::bind_params(&mut stmt, params)?;
            }
            return self.execute(stmt);
        }
        let mut stmts = xytalk_parser::parse_multi(input)?;
        if needs_bind {
            for s in stmts.iter_mut() {
                Self::bind_params(s, params)?;
            }
        }
        if stmts.len() == 1 {
            return self.execute(stmts.into_iter().next().unwrap());
        }
        self.execute_batch(stmts)
    }

    /// Substitute every `$param` placeholder in `stmt` with its bound value,
    /// erroring on any unbound parameter. The outer `match` is exhaustive (no
    /// wildcard) so a future statement variant carrying literals forces a
    /// compile error here rather than a silent anti-injection gap.
    fn bind_params(
        stmt: &mut Statement,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        use xytalk_parser::ast::PipelineStep as Ps;
        match stmt {
            Statement::Put(p) => Self::bind_fields(&mut p.fields, params)?,
            Statement::PutBatch(p) => {
                for rec in p.records.iter_mut() {
                    Self::bind_fields(rec, params)?;
                }
            }
            Statement::Find(f) => Self::bind_filters(&mut f.filters, params)?,
            Statement::Scan(s) => Self::bind_filter_expr_opt(&mut s.filter_expr, params)?,
            Statement::Set(s) => Self::bind_set(&mut s.assignments, &mut s.filter_expr, params)?,
            Statement::Delete(d) => Self::bind_filter_expr_opt(&mut d.filter_expr, params)?,
            Statement::Fetch(f) => Self::bind_filter_expr_opt(&mut f.filter_expr, params)?,
            Statement::Link(l) => {
                Self::bind_filter_expr_opt(&mut l.source_filter_expr, params)?;
                Self::bind_filter_expr_opt(&mut l.target_filter_expr, params)?;
            }
            Statement::CreateGhost(g) => Self::bind_filter_expr(&mut g.filter, params)?,
            Statement::ScanGhost(g) => Self::bind_filter_expr_opt(&mut g.filter_expr, params)?,
            Statement::InCache(c) => Self::bind_filter_expr_opt(&mut c.filter_expr, params)?,
            Statement::Pipeline(steps) => {
                for step in steps.iter_mut() {
                    match step {
                        Ps::Find(f) => Self::bind_filters(&mut f.filters, params)?,
                        Ps::Scan(s) => Self::bind_filter_expr_opt(&mut s.filter_expr, params)?,
                        Ps::ScanGhost(g) => Self::bind_filter_expr_opt(&mut g.filter_expr, params)?,
                        Ps::Set(s) => {
                            Self::bind_set(&mut s.assignments, &mut s.filter_expr, params)?
                        }
                        Ps::Delete(d) => Self::bind_filter_expr_opt(&mut d.filter_expr, params)?,
                        Ps::Nearest(n) => Self::bind_nearest(n, params)?,
                        Ps::Pull(_)
                        | Ps::Aggregate(_)
                        | Ps::GroupBy(_)
                        | Ps::Shape(_)
                        | Ps::Follow(_)
                        | Ps::Top(_) => {}
                    }
                }
            }
            // No literal-bearing positions (names/flags only).
            Statement::Pull(_)
            | Statement::Anchor(_)
            | Statement::Gravity(_)
            | Statement::Vector(_)
            | Statement::Satellite(_)
            | Statement::Lobe(_)
            | Statement::Show(_)
            | Statement::AutoAnchorApply(_)
            | Statement::RefreshGhost(_)
            | Statement::DropGhost(_)
            | Statement::Analyze(_)
            | Statement::Compact
            | Statement::Scrub
            | Statement::BulkMode(_)
            | Statement::Migrate(_)
            | Statement::OutCache(_)
            | Statement::Pin(_)
            | Statement::Unpin(_)
            // PURGE carries only a lobe name, no literal-bearing positions.
            | Statement::Purge(_) => {}
        }
        Ok(())
    }

    fn bind_set(
        assignments: &mut [(String, xytalk_parser::ast::Literal)],
        filter_expr: &mut Option<xytalk_parser::ast::FilterExpr>,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        for (_, v) in assignments.iter_mut() {
            Self::bind_literal(v, params)?;
        }
        Self::bind_filter_expr_opt(filter_expr, params)
    }

    fn bind_fields(
        fields: &mut [xytalk_parser::ast::PutField],
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        for f in fields.iter_mut() {
            Self::bind_literal(&mut f.value, params)?;
        }
        Ok(())
    }

    fn bind_filters(
        filters: &mut [xytalk_parser::ast::Filter],
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        for f in filters.iter_mut() {
            Self::bind_literal(&mut f.value, params)?;
        }
        Ok(())
    }

    fn bind_filter_expr_opt(
        fe: &mut Option<xytalk_parser::ast::FilterExpr>,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        if let Some(fe) = fe {
            Self::bind_filter_expr(fe, params)?;
        }
        Ok(())
    }

    fn bind_filter_expr(
        fe: &mut xytalk_parser::ast::FilterExpr,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        use xytalk_parser::ast::FilterExpr;
        match fe {
            FilterExpr::Condition(f) => Self::bind_literal(&mut f.value, params)?,
            FilterExpr::And(v) | FilterExpr::Or(v) => {
                for e in v.iter_mut() {
                    Self::bind_filter_expr(e, params)?;
                }
            }
            FilterExpr::Not(b) => Self::bind_filter_expr(b, params)?,
        }
        Ok(())
    }

    /// Replace a `Param` literal with its bound value; recurse into list/map
    /// elements. Errors on an unbound or unsupported-type parameter.
    fn bind_literal(
        lit: &mut xytalk_parser::ast::Literal,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        use xytalk_parser::ast::Literal;
        match lit {
            Literal::Param(name) => {
                let name = name.clone();
                let value = params
                    .get(&name)
                    .ok_or_else(|| XyzError::InvalidQuery(format!("unbound parameter ${name}")))?;
                *lit = Self::value_to_literal(value, &name)?;
            }
            Literal::List(items) => {
                for it in items.iter_mut() {
                    Self::bind_literal(it, params)?;
                }
            }
            Literal::Map(pairs) => {
                for (_, v) in pairs.iter_mut() {
                    Self::bind_literal(v, params)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Bind a `NEAREST` query: a `$param` resolves to its bound vector; an
    /// inline `Vector(list)` may itself contain `$param` elements.
    fn bind_nearest(
        n: &mut xytalk_parser::ast::NearestStmt,
        params: &HashMap<String, xyzdb_core::value::Value>,
    ) -> Result<()> {
        use xytalk_parser::ast::{Literal, NearestQuery};
        match &mut n.query {
            NearestQuery::Param(name) => {
                let name = name.clone();
                let value = params.get(&name).ok_or_else(|| {
                    XyzError::InvalidQuery(format!("NEAREST: parameter ${name} not bound"))
                })?;
                let lit = Self::value_to_literal(value, &name)?;
                if !matches!(lit, Literal::List(_)) {
                    return Err(XyzError::InvalidQuery(format!(
                        "NEAREST: parameter ${name} must be a list of numbers"
                    )));
                }
                n.query = NearestQuery::Vector(lit);
            }
            NearestQuery::Vector(lit) => Self::bind_literal(lit, params)?,
            NearestQuery::Ref(_) => {}
        }
        Ok(())
    }

    /// Convert a bound `Value` into the AST `Literal` that replaces its `$param`.
    /// `Timestamp`/`Bytes` have no lossless literal form yet and error rather
    /// than convert imprecisely.
    fn value_to_literal(
        v: &xyzdb_core::value::Value,
        name: &str,
    ) -> Result<xytalk_parser::ast::Literal> {
        use xytalk_parser::ast::Literal;
        use xyzdb_core::value::Value;
        Ok(match v {
            Value::Bool(b) => Literal::Bool(*b),
            Value::Int(i) => Literal::Int(*i),
            Value::Float(f) => Literal::Float(*f),
            Value::Text(s) => Literal::Text(s.clone()),
            Value::Null => Literal::Null,
            Value::List(items) => Literal::List(
                items
                    .iter()
                    .map(|x| Self::value_to_literal(x, name))
                    .collect::<Result<Vec<Literal>>>()?,
            ),
            Value::Map(pairs) => Literal::Map(
                pairs
                    .iter()
                    .map(|(k, x)| Self::value_to_literal(x, name).map(|l| (k.clone(), l)))
                    .collect::<Result<Vec<(String, Literal)>>>()?,
            ),
            Value::Vector(packed) => {
                Literal::List(packed.iter().map(|x| Literal::Float(*x as f64)).collect())
            }
            Value::Timestamp(_) | Value::Bytes(_) => {
                return Err(XyzError::InvalidQuery(format!(
                    "parameter ${name}: type not bindable yet (use Text/Int/Float/Bool/List/Map)"
                )));
            }
        })
    }

    /// Execute multiple statements, auto-batching CREATE GHOST by source lobe.
    /// Executes COMPACT first, then batches CREATE GHOST by source lobe
    /// (single scan per lobe instead of N scans).
    fn execute_batch(&self, stmts: Vec<Statement>) -> Result<QueryResult> {
        use std::collections::BTreeMap;

        let mut ghost_stmts: BTreeMap<String, Vec<xytalk_parser::ast::CreateGhostStmt>> =
            BTreeMap::new();
        let mut has_compact = false;
        let mut other_stmts: Vec<Statement> = Vec::new();

        for stmt in stmts {
            match stmt {
                Statement::CreateGhost(gs) => {
                    ghost_stmts
                        .entry(gs.source_lobe.clone())
                        .or_default()
                        .push(gs);
                }
                Statement::Compact => has_compact = true,
                other => other_stmts.push(other),
            }
        }

        // Execute non-ghost, non-compact statements first (e.g., BULKMODE OFF)
        let mut last_result = QueryResult::Ok {
            lid: None,
            message: String::new(),
        };
        for stmt in other_stmts {
            last_result = self.execute(stmt)?;
        }

        if has_compact {
            last_result = self.execute_compact()?;
        }

        // Batch CREATE GHOST by source lobe (single scan per lobe)
        for (lobe, ghost_list) in ghost_stmts {
            if ghost_list.len() == 1 {
                let gs = ghost_list.into_iter().next().unwrap();
                last_result = self.execute(Statement::CreateGhost(gs))?;
            } else {
                last_result = self.execute_create_ghost_batch(&lobe, ghost_list)?;
            }
        }

        Ok(last_result)
    }

    /// Execute a pre-parsed statement. Used by the server for streaming dispatch.
    pub fn execute_statement(&self, stmt: Statement) -> Result<QueryResult> {
        self.execute(stmt)
    }
}

/// True if a statement reads or writes gravity-placed records, so it must be
/// blocked while the database holds un-migrated pre-D1 gravity data (the D1
/// guard in [`Engine::execute`]). Admin/DDL — incl. `migrate`, `LOBE`,
/// `GRAVITY BY`, ghost defs, `SHOW`, compaction — stays open so the operator can
/// rehash. Conservative: anything that touches the spatial keyspace by gravity
/// bucket (PUT/SCAN/FIND/PULL/SET/DELETE/LINK/PLACE/SCAN GHOST) is blocked.
fn stmt_touches_gravity_data(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Put(_)
            | Statement::PutBatch(_)
            | Statement::Find(_)
            | Statement::Pull(_)
            | Statement::Scan(_)
            | Statement::Set(_)
            | Statement::Delete(_)
            | Statement::Purge(_)
            | Statement::Fetch(_)
            | Statement::Link(_)
            | Statement::ScanGhost(_)
    )
}

/// Point administrative statements at their supported surface,
/// `xyzdb-cli admin <verb>`, without promising a retirement.
///
/// The language forms are **permanent aliases**: drivers, benchmarks and
/// validation suites in the wild send them, and nothing is gained by
/// breaking those. Earlier versions of this warning announced removal in
/// v0.3.0 and kept announcing it through 1.1.0, which is worse than no
/// warning at all — an operator plans a migration that never arrives. The
/// recommendation is worth keeping; the deadline was not.
///
/// Server-side `tracing` only; no `QueryResult` shape change.
fn warn_admin_statement_deprecated(stmt_upper: &str, cli_verb: &str) {
    tracing::warn!(
        "Statement {stmt_upper} is an administrative operation; \
         prefer 'xyzdb-cli admin {cli_verb}' so it stays out of application \
         query paths. The statement form keeps working — no removal is planned."
    );
}
