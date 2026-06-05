use super::*;

impl WorkDb {
    pub fn open(path: PathBuf) -> Result<Self> {
        if path == Path::new(":memory:") {
            return Self::open_in_memory();
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create work db directory {}", parent.display())
            })?;
        }

        let db = Self { path, memory: None };
        db.init()?;
        Ok(db)
    }

    /// Create a per-call named shared-cache in-memory database. Each call
    /// gets a unique name so parallel tests never share state. The anchor
    /// connection keeps the database alive until the `WorkDb` is dropped.
    pub(crate) fn open_in_memory() -> Result<Self> {
        let id = NEXT_MEM_DB_ID.fetch_add(1, Ordering::Relaxed);
        let uri = format!("file:boss_mem_{id}?mode=memory&cache=shared");
        let anchor = Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("failed to open in-memory db {uri}"))?;
        let db = Self {
            path: PathBuf::from(":memory:"),
            memory: Some(InMemoryAnchor {
                uri,
                _conn: Arc::new(Mutex::new(anchor)),
            }),
        };
        db.init()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_in_memory(&self) -> bool {
        self.memory.is_some()
    }

    /// Find the id of the first product whose `repo_remote_url` matches the
    /// given canonical URL. Returns `None` when no product matches.
    /// Used by the Phase-4 magic-wand PR-backed dispatch to resolve the
    /// product for the spawned chore.
    pub fn find_product_id_by_repo_remote_url(&self, url: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        find_product_by_repo_remote_url(&conn, url)
    }

    pub fn list_products(&self) -> Result<Vec<Product>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model, dispatch_preamble, external_tracker_kind, external_tracker_config, design_repo, docs_repo, worker_branch_prefix, editorial_rules
             FROM products
             ORDER BY name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], map_product)?;
        collect_rows(rows)
    }

    pub fn create_product(&self, input: CreateProductInput) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let id = next_id("prod");
        let now = now_string();
        let slug = unique_product_slug(&tx, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let repo_remote_url = canonicalize_repo_remote_url(input.repo_remote_url);
        let design_repo = canonicalize_repo_remote_url(input.design_repo);
        let docs_repo = canonicalize_repo_remote_url(input.docs_repo);
        let worker_branch_prefix = canonicalize_worker_branch_prefix(input.worker_branch_prefix);

        tx.execute(
            "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model, design_repo, docs_repo, worker_branch_prefix)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6, NULL, ?7, ?8, ?9)",
            params![id, input.name, slug, description, repo_remote_url, now, design_repo, docs_repo, worker_branch_prefix],
        )?;

        let product = query_product(&tx, &id)?
            .with_context(|| format!("missing product after insert: {id}"))?;
        tx.commit()?;
        Ok(product)
    }

    pub fn list_projects(
        &self,
        product_id: &str,
        dep_filter: Option<&DependencyFilter>,
    ) -> Result<Vec<Project>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                    design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
             FROM projects
             WHERE product_id = ?1
             ORDER BY created_at ASC, name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([product_id], map_project)?;
        let mut projects: Vec<Project> = collect_rows(rows)?;
        if let Some(filter) = dep_filter {
            apply_dep_filter(
                &conn,
                filter,
                |project: &Project| project.id.as_str(),
                |project: &Project| project.status.as_str(),
                &mut projects,
            )?;
        }
        Ok(projects)
    }

    pub fn create_project(&self, input: CreateProjectInput) -> Result<Project> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("proj");
        let now = now_string();
        let slug = unique_project_slug(&tx, &input.product_id, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let goal = input.goal.unwrap_or_default();
        let short_id = allocate_short_id(&tx, &input.product_id)?;

        tx.execute(
            "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, short_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planned', 'medium', ?7, ?7, ?8)",
            params![id, input.product_id, input.name, slug, description, goal, now, short_id],
        )?;

        // Auto-create the project's design task unless the caller
        // opted out with `no_design_task`. For design-shaped projects
        // the task sorts first (ordinal = 0) so the dispatcher picks
        // it up before the project's own tasks (ordinal ≥ 1).
        // Non-design-shaped projects (postmortems, checklists, etc.)
        // pass `no_design_task = true` and land here with zero tasks.
        if !input.no_design_task {
            insert_design_task_for_project_in_tx(
                &tx,
                &input.product_id,
                &id,
                &input.name,
                input.autostart,
            )?;
        }

        let project = query_project(&tx, &id)?
            .with_context(|| format!("missing project after insert: {id}"))?;
        tx.commit()?;
        Ok(project)
    }

    pub fn create_task(&self, input: CreateTaskInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let task = insert_task_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(task)
    }

    pub fn create_chore(&self, input: CreateChoreInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let chore = insert_chore_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(chore)
    }

    /// Create a `kind = 'investigation'` task. Parallel to `create_chore`
    /// but uses the `investigation` kind and supports an optional `project_id`.
    pub fn create_investigation(
        &self,
        input: boss_protocol::CreateInvestigationInput,
    ) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let task = insert_investigation_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(task)
    }

    /// Create a `kind = 'revision'` task bound to an existing parent task.
    /// Runs the create-time gate (`assert_parent_revisable`) to confirm the
    /// chain root's PR is open and unmerged before inserting.
    ///
    /// `pr_checker` supplies the live PR state for chains where the cached
    /// DB state alone cannot distinguish open from closed-unmerged. Pass
    /// `&GhPrStateChecker` in production; pass `&FakePrStateChecker` in tests.
    pub fn create_revision(
        &self,
        input: CreateRevisionInput,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = assert_parent_revisable_and_insert(&tx, input, pr_checker)?;
        tx.commit()?;
        Ok(task)
    }

    /// Create a chore and immediately bind it to an upstream tracker reference
    /// in a single SQLite transaction.
    ///
    /// The external-tracker importer must use this method instead of a
    /// separate `create_chore` + `set_external_ref` pair. The two-step
    /// approach leaves a window where an engine crash produces a chore with
    /// `external_ref_canonical_id = NULL`; on the next reconcile tick the
    /// orphaned chore is invisible to `find_by_external_ref`, the reconciler
    /// re-imports the upstream item as a duplicate, and the original chore
    /// never participates in reverse-close or forward-transition flows.
    ///
    /// Also bumps `external_ref_synced_at` within the same transaction so the
    /// row is fully ready for the reconciler immediately after commit.
    ///
    /// `upstream_title` and `upstream_body` are the raw upstream issue title
    /// and body (not the formatted description). SHA-256 checksums of both the
    /// upstream content and the initial boss name/description are stored as
    /// the Behavior 8 drift-detection baseline.
    pub fn import_chore_with_external_ref(
        &self,
        input: CreateChoreInput,
        kind: &str,
        canonical_id: &str,
        raw: &serde_json::Value,
        upstream_title: &str,
        upstream_body: &str,
    ) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let chore = insert_chore_in_tx(&tx, input)?;
        let upstream_checksum = content_checksum(upstream_title, upstream_body);
        let boss_checksum = content_checksum(&chore.name, &chore.description);
        let raw_json = serde_json::to_string(raw)
            .with_context(|| format!("failed to serialise external_ref raw for {}", chore.id))?;
        let now = now_string();
        tx.execute(
            "UPDATE tasks
             SET external_ref_kind                  = ?2,
                 external_ref_canonical_id          = ?3,
                 external_ref_raw                   = ?4,
                 external_ref_synced_at             = ?5,
                 external_ref_unbound_at            = NULL,
                 external_ref_upstream_checksum     = ?6,
                 external_ref_boss_checksum         = ?7,
                 updated_at                         = ?5
             WHERE id = ?1 AND deleted_at IS NULL",
            params![chore.id, kind, canonical_id, raw_json, now, upstream_checksum, boss_checksum],
        )?;
        tx.commit()?;
        Ok(chore)
    }

    /// Insert N tasks atomically. The whole batch is wrapped in a
    /// single sqlite transaction; any per-item validation failure
    /// rolls back the entire batch (no partial state). Errors are
    /// annotated with the offending item index so the CLI can map
    /// them back to the input file.
    pub fn create_many_tasks(&self, input: CreateManyTasksInput) -> Result<Vec<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut created = Vec::with_capacity(input.items.len());
        for (index, item) in input.items.into_iter().enumerate() {
            let task = insert_task_in_tx(&tx, item).with_context(|| format!("item {index}"))?;
            created.push(task);
        }
        tx.commit()?;
        Ok(created)
    }

    /// Insert N chores atomically. See [`Self::create_many_tasks`] for
    /// atomicity contract.
    pub fn create_many_chores(&self, input: CreateManyChoresInput) -> Result<Vec<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut created = Vec::with_capacity(input.items.len());
        for (index, item) in input.items.into_iter().enumerate() {
            let chore = insert_chore_in_tx(&tx, item).with_context(|| format!("item {index}"))?;
            created.push(chore);
        }
        tx.commit()?;
        Ok(created)
    }

    pub fn create_execution(&self, input: CreateExecutionInput) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = insert_execution(&tx, input)?;
        tx.commit()?;
        Ok(execution)
    }
}
