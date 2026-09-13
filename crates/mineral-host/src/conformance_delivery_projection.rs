//! Real-Git conformance for the delivery stage.
//!
//! These tests exist to prove one boundary against a real repository rather than
//! against a fake: the Git target is built from the delivery text side, so a
//! binary asset a public document references can never reach the reviewed tree,
//! while the document that references it carries the final HTTPS URL.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use crate::{
    asset::{
        AssetObservationId, AssetObservationStore, AssetTarget, ConfiguredAssetTarget,
        FilesystemAssetTarget, R2ObjectStore, R2ObjectStoreConfig, R2SecretKey,
        SequentialAssetObservationIdGenerator,
    },
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId, SourceId, TimestampMillis},
    publisher::{
        DeliveryProjectionBinding, GitCommitMetadata, GitCommitObjectCreator, GitCommitOid,
        GitCommitSpec, GitProjectionMaterializer, GitPublicationApplication,
        GitPublicationPrepareRequest, GitPublicationPreparer, GitRefTarget, GitRepositoryAdapter,
        GitTreeOid, PublishRun, PublishRunId, PublishRunStore, PublishTargetId,
        RemoteObservationId, RepositoryLocator, SequentialPublishRunIdGenerator,
        SequentialRemoteObservationIdGenerator,
    },
    storage::{
        LocalContentStore, SqliteAssetObservationStore, SqliteDeliveryProjectionStore,
        SqlitePublishRunStore, SqliteRemoteObservationStore,
    },
    workflow::{
        AssetContentType, AssetDeliveryConfig, AssetReviewRunId, DeliveryProjection,
        DeliveryProjectionBuilder, DeliveryProjectionStore, FinalPublicationSet, ManagedRoot,
        PublicProjection, SanitizationTransformation, SanitizedAsset,
    },
};

static NEXT_REPOSITORY: AtomicU64 = AtomicU64::new(1);

const NOTE: &str = "# Note\n\n![[asset.png]]\n";
const ASSET: &[u8] = b"\x89PNG\r\n\x1a\nbinary attachment bytes";

struct TestRepository {
    path: PathBuf,
    local: PathBuf,
    remote: PathBuf,
    store: LocalContentStore,
}

impl TestRepository {
    fn new() -> Option<Self> {
        if !Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return None;
        }
        let sequence = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mineral-publisher-delivery-conformance-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        let local = path.join("local");
        let remote = path.join("remote.git");
        let repository = Self {
            store: LocalContentStore::new(path.join("cas")),
            path: path.clone(),
            local: local.clone(),
            remote: remote.clone(),
        };
        let _ = &repository.remote;
        repository.git_in(&path, ["init", "--bare", remote.to_str().unwrap()]);
        repository.git_in(&path, ["init", local.to_str().unwrap()]);
        repository.git_in(&local, ["config", "user.name", "Mineral Publisher Tests"]);
        repository.git_in(&local, ["config", "user.email", "tests@mineral.invalid"]);
        fs::create_dir_all(local.join("content")).unwrap();
        fs::write(local.join("content/old.md"), b"old").unwrap();
        repository.git_in(&local, ["add", "-A"]);
        repository.git_in(&local, ["commit", "--quiet", "-m", "base"]);
        repository.git_in(&local, ["branch", "-M", "main"]);
        repository.git_in(
            &local,
            ["remote", "add", "origin", remote.to_str().unwrap()],
        );
        repository.git_in(&local, ["push", "-u", "origin", "main"]);
        Some(repository)
    }

    /// Runs one `git` command in the local worktree.
    fn git<const N: usize>(&self, args: [&str; N]) -> Vec<u8> {
        self.git_in(&self.local, args)
    }

    fn git_in<const N: usize>(&self, directory: &Path, args: [&str; N]) -> Vec<u8> {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn object_exists(&self, revision: &str) -> bool {
        Command::new("git")
            .current_dir(&self.local)
            .args(["cat-file", "-e", revision])
            .output()
            .unwrap()
            .status
            .success()
    }

    /// Deletes one loose object, modelling a runtime that lost part of its object
    /// database. Packed objects are never touched, so the test skips instead of
    /// reporting a false success.
    fn remove_loose_object(&self, oid: &str) -> bool {
        let path = self
            .local
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]);
        match fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("could not remove loose object {oid}: {error}"),
        }
    }

    fn git_ok<const N: usize>(&self, args: [&str; N]) -> bool {
        Command::new("git")
            .current_dir(&self.local)
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    }

    fn head(&self) -> String {
        String::from_utf8(self.git(["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn remote_head(&self) -> String {
        let output = Command::new("git")
            .current_dir(&self.local)
            .args(["ls-remote", "origin", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }

    fn tree_file(&self, tree: &str, path: &str) -> Vec<u8> {
        let spec = format!("{tree}:{path}");
        self.git(["show", &spec])
    }

    fn tree_paths(&self, tree: &str) -> String {
        String::from_utf8(self.git(["ls-tree", "-r", "--name-only", tree])).unwrap()
    }

    /// The exact public projection the review pipeline would hand to delivery.
    fn projection(&self) -> (Snapshot, PublicProjection) {
        let note = self.store.store(NOTE.as_bytes()).unwrap();
        let asset = self.store.store(ASSET).unwrap();
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![
                SnapshotFile::new(
                    ContentPath::new("note.md").unwrap(),
                    NOTE.len() as u64,
                    note,
                    None,
                ),
                SnapshotFile::new(
                    ContentPath::new("asset.png").unwrap(),
                    ASSET.len() as u64,
                    asset,
                    None,
                ),
            ],
        )
        .unwrap();
        let set = FinalPublicationSet::from_parts_for_test(
            snapshot.id(),
            vec![ContentPath::new("note.md").unwrap()],
            // The asset bytes are published unchanged in this fixture, so source
            // and published identity coincide.
            vec![SanitizedAsset::from_parts(
                ContentPath::new("asset.png").unwrap(),
                AssetReviewRunId::new(1).unwrap(),
                asset,
                asset,
                ASSET.len() as u64,
                png_content_type(),
                vec![SanitizationTransformation::Identity],
            )],
        );
        let projection =
            PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap();
        (snapshot, projection)
    }
}

impl Drop for TestRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn config() -> AssetDeliveryConfig {
    AssetDeliveryConfig::new("https://assets.example.com").unwrap()
}

/// The live test publishes with an empty public scope: the whole fixture vault is
/// in public scope, which is what these tests are about.
fn live_public_scope() -> crate::workflow::PublicExclusionRules {
    crate::workflow::PublicExclusionRules::empty()
}

/// The real bucket, when one is configured.
///
/// The default test run never reads a credential and never opens a socket; this
/// is only reached from the ignored live test below.
fn live_r2_target(spool: PathBuf) -> Option<ConfiguredAssetTarget> {
    let endpoint = std::env::var("MINERAL_R2_ENDPOINT").ok()?;
    let bucket = std::env::var("MINERAL_R2_BUCKET").ok()?;
    let access_key_id = std::env::var("MINERAL_R2_ACCESS_KEY_ID").ok()?;
    let secret_access_key = std::env::var("MINERAL_R2_SECRET_ACCESS_KEY").ok()?;
    let config = R2ObjectStoreConfig::new(
        endpoint,
        bucket,
        access_key_id,
        R2SecretKey::new(secret_access_key).unwrap(),
    )
    .unwrap();
    Some(ConfiguredAssetTarget::r2(
        R2ObjectStore::new(config, spool).expect("the spool directory must be usable"),
    ))
}

fn asset_target(repository: &TestRepository) -> FilesystemAssetTarget {
    FilesystemAssetTarget::new(repository.path.join("asset-target"))
}

fn png_content_type() -> AssetContentType {
    AssetContentType::new("image/png").unwrap()
}

fn prepare_request<'a>(
    delivery: &'a DeliveryProjection,
    base: &'a GitCommitOid,
    target_id: &'a PublishTargetId,
    locator: &'a RepositoryLocator,
    target: &'a GitRefTarget,
) -> GitPublicationPrepareRequest<'a> {
    GitPublicationPrepareRequest {
        id: PublishRunId::new(1).unwrap(),
        target_id,
        repository: locator,
        target,
        text_projection: delivery.text(),
        delivery: DeliveryProjectionBinding::from_projection(delivery),
        observed_base: base,
        author_name: "Mineral Publisher",
        author_email: "publisher@example.invalid",
        message: "Publish Mineral content",
        created_at: TimestampMillis::from_unix_millis(1_500),
    }
}

#[test]
fn git_target_holds_only_rewritten_documents_and_never_a_referenced_binary() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let (snapshot, projection) = repository.projection();
    let delivery =
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap();

    // The delivery keeps the asset on the storage side and pre-commits to exactly
    // one object identity for it.
    assert_eq!(delivery.assets().len(), 1);
    let asset = &delivery.assets().assets()[0];
    assert_eq!(asset.logical_path().as_str(), "asset.png");
    let url = asset.public_url().as_str().to_owned();
    assert!(url.starts_with("https://assets.example.com/assets/sha256/"));

    let base = GitCommitOid::new(repository.head()).unwrap();
    let target_id = PublishTargetId::new("origin:refs/heads/main").unwrap();
    let locator = RepositoryLocator::new(repository.local.to_str().unwrap()).unwrap();
    let target = GitRefTarget::new("origin", "refs/heads/main").unwrap();
    let adapter = GitRepositoryAdapter::new(&repository.local, &repository.store).unwrap();

    let preparation = GitPublicationPreparer::prepare(
        &adapter,
        &prepare_request(&delivery, &base, &target_id, &locator, &target),
    )
    .unwrap();

    let run = preparation.publish_run();
    let tree = run.reviewed_tree().to_owned();
    assert!(
        run.desired_commit().is_some(),
        "a real change creates a commit"
    );
    assert_ne!(tree, repository.head(), "the managed subtree changed");

    // Only the document is in the Git target.
    assert_eq!(repository.tree_paths(&tree), "content/note.md\n");
    assert!(
        !repository.git_ok(["cat-file", "-e", &format!("{tree}:content/asset.png")]),
        "a binary asset must never be committed"
    );
    assert!(
        !repository.git_ok(["cat-file", "-e", &format!("{tree}:content/old.md")]),
        "the complete managed subtree is the delivery text side"
    );

    // The committed document carries the final HTTPS URL, not the wikilink.
    let committed = String::from_utf8(repository.tree_file(&tree, "content/note.md")).unwrap();
    assert_eq!(committed, format!("# Note\n\n![]({url})\n"));
    assert!(!committed.contains("![["));
    // The filename appears exactly once, as the last segment of the delivered URL:
    // the document no longer resolves the asset by its vault location.
    assert_eq!(committed.matches("asset.png").count(), 1);
    assert!(url.ends_with("/asset.png"));
}

#[test]
fn the_reviewed_tree_identity_is_the_delivered_text_identity() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let (snapshot, projection) = repository.projection();
    let delivery =
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap();

    let base = GitCommitOid::new(repository.head()).unwrap();
    let target_id = PublishTargetId::new("origin:refs/heads/main").unwrap();
    let locator = RepositoryLocator::new(repository.local.to_str().unwrap()).unwrap();
    let target = GitRefTarget::new("origin", "refs/heads/main").unwrap();
    let adapter = GitRepositoryAdapter::new(&repository.local, &repository.store).unwrap();
    let request = prepare_request(&delivery, &base, &target_id, &locator, &target);

    let first = GitPublicationPreparer::prepare(&adapter, &request).unwrap();
    let second = GitPublicationPreparer::prepare(&adapter, &request).unwrap();

    assert_eq!(
        first.publish_run().reviewed_tree(),
        second.publish_run().reviewed_tree()
    );
    assert_eq!(
        first.publish_run().reviewed_text_projection_sha256(),
        Some(delivery.text().projection_sha256())
    );
    assert_eq!(
        first.publish_run().delivery_projection_binding(),
        Some(DeliveryProjectionBinding::from_projection(&delivery))
    );
    // The reviewed-tree identity is the delivered text identity, and the public
    // content identity stays separately auditable because the committed bytes are
    // the rewritten ones.
    assert_ne!(
        first.publish_run().reviewed_text_projection_sha256(),
        Some(projection.projection_sha256())
    );
    assert_eq!(
        delivery.source_projection_sha256(),
        projection.projection_sha256()
    );
    assert_eq!(
        delivery.delivery_sha256(),
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap()
            .delivery_sha256()
    );
}

#[test]
fn a_binary_with_no_public_document_reference_never_reaches_git_or_delivery() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let note = repository.store.store(NOTE.as_bytes()).unwrap();
    let asset = repository.store.store(ASSET).unwrap();
    let orphan = repository.store.store(b"orphan bytes").unwrap();
    let snapshot = Snapshot::new(
        SnapshotId::new(1).unwrap(),
        SystemTime::UNIX_EPOCH,
        SourceId::new("test").unwrap(),
        vec![
            SnapshotFile::new(
                ContentPath::new("note.md").unwrap(),
                NOTE.len() as u64,
                note,
                None,
            ),
            SnapshotFile::new(
                ContentPath::new("asset.png").unwrap(),
                ASSET.len() as u64,
                asset,
                None,
            ),
            SnapshotFile::new(ContentPath::new("orphan.png").unwrap(), 12, orphan, None),
        ],
    )
    .unwrap();
    let set = FinalPublicationSet::from_parts_for_test(
        snapshot.id(),
        vec![ContentPath::new("note.md").unwrap()],
        vec![
            SanitizedAsset::from_parts(
                ContentPath::new("asset.png").unwrap(),
                AssetReviewRunId::new(1).unwrap(),
                asset,
                asset,
                ASSET.len() as u64,
                png_content_type(),
                vec![SanitizationTransformation::Identity],
            ),
            SanitizedAsset::from_parts(
                ContentPath::new("orphan.png").unwrap(),
                AssetReviewRunId::new(2).unwrap(),
                orphan,
                orphan,
                12,
                png_content_type(),
                vec![SanitizationTransformation::Identity],
            ),
        ],
    );
    let projection =
        PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap();

    let delivery =
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap();

    assert_eq!(delivery.assets().len(), 1);
    assert!(
        delivery
            .assets()
            .get(&ContentPath::new("orphan.png").unwrap())
            .is_none()
    );
    assert_eq!(
        Sha256::digest(ASSET),
        delivery.assets().assets()[0].published_sha256()
    );
    assert_eq!(
        delivery
            .text()
            .files()
            .iter()
            .map(|file| file.target_path().as_str())
            .collect::<Vec<_>>(),
        ["content/note.md"]
    );
    assert!(
        delivery.assets().assets()[0]
            .public_url()
            .as_str()
            .starts_with("https://assets.example.com/")
    );
}

/// §10 against a real repository: an object database that never held the reviewed
/// tree can still publish, because the tree and then the commit are rebuilt from
/// the durable delivery projection the intent bound.
#[test]
fn a_runtime_that_lost_its_objects_republishes_from_the_durable_projection() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let (snapshot, projection) = repository.projection();
    let delivery =
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap();

    // The delivery intent becomes durable before anything else, exactly as the
    // composition root orders it.
    let deliveries =
        SqliteDeliveryProjectionStore::open(repository.path.join("deliveries.sqlite")).unwrap();
    assert_eq!(deliveries.get(delivery.delivery_sha256()).unwrap(), None);
    DeliveryProjectionStore::save(&deliveries, &delivery).unwrap();

    let base = GitCommitOid::new(repository.head()).unwrap();
    // Rebuild the reviewed tree once to learn its real Git identity, then freeze
    // the commit identity that must be recreated from the stored projection.
    let reviewed = GitProjectionMaterializer::materialize(
        &repository.local,
        base.as_str(),
        delivery.text(),
        &repository.store,
    )
    .unwrap();
    let spec = GitCommitSpec::new(
        base.clone(),
        GitTreeOid::new(reviewed.tree_oid()).unwrap(),
        "Mineral Publisher",
        "publisher@example.invalid",
        TimestampMillis::from_unix_millis(1_500),
        "Mineral Publisher",
        "publisher@example.invalid",
        TimestampMillis::from_unix_millis(1_500),
        "Publish Mineral content",
    )
    .unwrap();
    let desired_commit =
        GitCommitObjectCreator::create_from_spec(&repository.local, &spec).unwrap();

    let run = PublishRun::rehydrate(
        PublishRunId::new(1).unwrap(),
        snapshot.id(),
        None,
        Some(delivery.text().projection_sha256()),
        Some(delivery.delivery_sha256()),
        ManagedRoot::new("content").unwrap(),
        PublishTargetId::new("origin:refs/heads/main").unwrap(),
        RepositoryLocator::new(repository.local.to_str().unwrap()).unwrap(),
        GitRefTarget::new("origin", "refs/heads/main").unwrap(),
        base.as_str().to_owned(),
        reviewed.tree_oid().to_owned(),
        Some(desired_commit.clone()),
        Some(spec.clone()),
        1_500,
    )
    .unwrap();
    let runs = SqlitePublishRunStore::open(repository.path.join("runs.sqlite")).unwrap();
    PublishRunStore::save(&runs, &run).unwrap();
    assert_eq!(repository.remote_head(), base.as_str());

    // The object database loses the reviewed tree and the commit. Recreating the
    // commit from its frozen specification alone is now impossible — the runtime
    // must rebuild the tree first, from the durable projection and nothing else.
    assert!(repository.remove_loose_object(reviewed.tree_oid()));
    assert!(repository.remove_loose_object(desired_commit.as_str()));
    assert!(!repository.object_exists(reviewed.tree_oid()));
    assert!(!repository.object_exists(desired_commit.as_str()));
    assert!(
        GitCommitObjectCreator::create_from_spec(&repository.local, &spec).is_err(),
        "a commit cannot be recreated while its tree is gone"
    );

    let observations =
        SqliteRemoteObservationStore::open(repository.path.join("observations.sqlite")).unwrap();
    let asset_observations =
        SqliteAssetObservationStore::open(repository.path.join("asset-observations.sqlite"))
            .unwrap();
    let mut observation_ids =
        SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());
    let mut asset_observation_ids =
        SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
    let execution = GitPublicationApplication::resume(
        run.id(),
        &runs,
        &deliveries,
        &asset_target(&repository),
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut observation_ids,
        &repository.store,
    )
    .unwrap();

    assert!(
        execution.is_satisfied(),
        "recovery must publish, got {execution:?}"
    );
    // The reviewed tree is back, byte for byte, and the ref moved to the commit
    // that was frozen before the objects were lost.
    assert_eq!(
        repository.tree_paths(reviewed.tree_oid()),
        "content/note.md\n"
    );
    assert!(repository.object_exists(&format!("{}^{{commit}}", desired_commit.as_str())));
    assert_eq!(repository.remote_head(), desired_commit.as_str());

    // The committed document is the rewritten one, exactly as delivered.
    let committed =
        String::from_utf8(repository.tree_file(reviewed.tree_oid(), "content/note.md")).unwrap();
    assert_eq!(
        committed,
        format!(
            "# Note\n\n![]({})\n",
            delivery.assets().assets()[0].public_url()
        )
    );

    // Recovery read the bound projection and never captured a second one.
    assert_eq!(
        deliveries.get(delivery.delivery_sha256()).unwrap(),
        Some(delivery)
    );
}

/// §3/§10: the exact Git compare-and-swap may not happen until every required
/// asset is verified on the asset target, proven against a real repository, a real
/// filesystem object target, real SQLite stores, and the real CAS.
#[test]
fn a_broken_asset_keeps_a_real_publication_off_the_remote() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let (snapshot, projection) = repository.projection();
    let delivery =
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap();
    let target_root = repository.path.join("asset-target");
    let target = FilesystemAssetTarget::new(&target_root);

    let deliveries =
        SqliteDeliveryProjectionStore::open(repository.path.join("deliveries.sqlite")).unwrap();
    DeliveryProjectionStore::save(&deliveries, &delivery).unwrap();

    let base = GitCommitOid::new(repository.head()).unwrap();
    let reviewed = GitProjectionMaterializer::materialize(
        &repository.local,
        base.as_str(),
        delivery.text(),
        &repository.store,
    )
    .unwrap();
    let spec = GitCommitSpec::new(
        base.clone(),
        GitTreeOid::new(reviewed.tree_oid()).unwrap(),
        "Mineral Publisher",
        "publisher@example.invalid",
        TimestampMillis::from_unix_millis(1_500),
        "Mineral Publisher",
        "publisher@example.invalid",
        TimestampMillis::from_unix_millis(1_500),
        "Publish Mineral content",
    )
    .unwrap();
    let desired_commit =
        GitCommitObjectCreator::create_from_spec(&repository.local, &spec).unwrap();
    let run = PublishRun::rehydrate(
        PublishRunId::new(1).unwrap(),
        snapshot.id(),
        None,
        Some(delivery.text().projection_sha256()),
        Some(delivery.delivery_sha256()),
        ManagedRoot::new("content").unwrap(),
        PublishTargetId::new("origin:refs/heads/main").unwrap(),
        RepositoryLocator::new(repository.local.to_str().unwrap()).unwrap(),
        GitRefTarget::new("origin", "refs/heads/main").unwrap(),
        base.as_str().to_owned(),
        reviewed.tree_oid().to_owned(),
        Some(desired_commit.clone()),
        Some(spec.clone()),
        1_500,
    )
    .unwrap();
    let runs = SqlitePublishRunStore::open(repository.path.join("runs.sqlite")).unwrap();
    PublishRunStore::save(&runs, &run).unwrap();
    let observations =
        SqliteRemoteObservationStore::open(repository.path.join("observations.sqlite")).unwrap();
    let asset_observations =
        SqliteAssetObservationStore::open(repository.path.join("asset-observations.sqlite"))
            .unwrap();
    let mut observation_ids =
        SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());
    let mut asset_observation_ids =
        SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());

    // The immutable content store loses the published asset blob: the runtime
    // cannot produce the frozen representation any more.
    let published_sha256 = delivery.assets().assets()[0].published_sha256();
    let cas_blob = repository.store.root().join(published_sha256.to_string());
    let published_bytes = fs::read(&cas_blob).unwrap();
    fs::remove_file(&cas_blob).unwrap();
    assert!(repository.remote_head() == base.as_str());

    let error = GitPublicationApplication::resume(
        run.id(),
        &runs,
        &deliveries,
        &target,
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut observation_ids,
        &repository.store,
    )
    .unwrap_err();

    // The Git target never moved, and the asset target still holds nothing.
    assert!(format!("{error}").contains("required assets"), "{error}");
    assert_eq!(repository.remote_head(), base.as_str());
    assert!(matches!(
        target
            .inspect(delivery.assets().assets()[0].object_key())
            .unwrap(),
        crate::asset::AssetTargetState::Missing
    ));
    // The missing observation was still recorded before the attempt refused.
    assert_eq!(
        asset_observations
            .list_for_publish_run(run.id())
            .unwrap()
            .len(),
        1
    );

    // Restoring the frozen representation lets the same attempt finish: the asset
    // is published, verified, and only then does the ref move.
    fs::write(&cas_blob, &published_bytes).unwrap();
    let execution = GitPublicationApplication::resume(
        run.id(),
        &runs,
        &deliveries,
        &target,
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut observation_ids,
        &repository.store,
    )
    .unwrap();

    assert!(execution.is_satisfied(), "got {execution:?}");
    assert_eq!(repository.remote_head(), desired_commit.as_str());
    assert!(repository.object_exists(&format!("{}^{{commit}}", desired_commit.as_str())));
    assert!(matches!(
        target
            .inspect(delivery.assets().assets()[0].object_key())
            .unwrap(),
        crate::asset::AssetTargetState::Present(_)
    ));
    assert_eq!(
        fs::read(
            target_root.join(
                delivery.assets().assets()[0]
                    .object_key()
                    .as_str()
                    .replace('/', std::path::MAIN_SEPARATOR_STR)
            )
        )
        .unwrap(),
        published_bytes
    );
}

/// The S6.4 path, end to end, against the bucket this workspace is configured to
/// publish to: one real delivery execution over a real repository, real SQLite
/// stores, the real `ConfiguredAssetTarget` and the real object store.
///
/// It runs only when `MINERAL_R2_ENDPOINT`, `MINERAL_R2_BUCKET`,
/// `MINERAL_R2_ACCESS_KEY_ID` and `MINERAL_R2_SECRET_ACCESS_KEY` name a bucket:
///
/// ```text
/// cargo test -p mineral-publisher --lib \
///   a_live_bucket_receives_the_asset_before_a_real_git_compare_and_swap \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a live bucket and credentials"]
fn a_live_bucket_receives_the_asset_before_a_real_git_compare_and_swap() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let Some(asset_target) = live_r2_target(repository.path.join("asset-spool")) else {
        panic!(
            "the live test needs MINERAL_R2_ENDPOINT, MINERAL_R2_BUCKET, MINERAL_R2_ACCESS_KEY_ID and MINERAL_R2_SECRET_ACCESS_KEY"
        );
    };
    let (snapshot, projection) = repository.projection();
    let deliveries =
        SqliteDeliveryProjectionStore::open(repository.path.join("deliveries.sqlite")).unwrap();
    let runs = SqlitePublishRunStore::open(repository.path.join("runs.sqlite")).unwrap();
    let observations =
        SqliteRemoteObservationStore::open(repository.path.join("observations.sqlite")).unwrap();
    let asset_observations =
        SqliteAssetObservationStore::open(repository.path.join("asset-observations.sqlite"))
            .unwrap();
    let mut publish_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
    let mut observation_ids =
        SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());
    let mut asset_observation_ids =
        SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
    let metadata = GitCommitMetadata::new(
        "Mineral Publisher",
        "publisher@example.invalid",
        "Publish Mineral content",
    )
    .unwrap();
    let target_id = PublishTargetId::new("origin:refs/heads/main").unwrap();
    let target = GitRefTarget::new("origin", "refs/heads/main").unwrap();
    let base = repository.remote_head();

    let first = GitPublicationApplication::prepare_and_publish(
        &projection,
        &snapshot,
        &live_public_scope(),
        &config(),
        &repository.local,
        target_id.clone(),
        target.clone(),
        &metadata,
        &repository.store,
        &runs,
        &deliveries,
        &asset_target,
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut publish_ids,
        &mut observation_ids,
    )
    .unwrap();

    let execution = first.workflow();
    let first_published = execution.assets().published();
    assert!(execution.is_satisfied(), "the delivery must be satisfied");
    // The key is content-addressed, so a bucket that already holds this object
    // (from an earlier run of this test, or from the same bytes published before)
    // verifies and reuses it instead of uploading. What must hold either way is
    // that the asset side is satisfied by bytes the bucket actually serves.
    assert_eq!(
        execution.assets().verified(),
        1,
        "the asset must be verified"
    );
    assert!(
        execution.assets().published() <= 1,
        "at most one object may be published"
    );
    eprintln!(
        "first run: verified={} published={}",
        execution.assets().verified(),
        execution.assets().published()
    );
    let published = repository.remote_head();
    assert_ne!(published, base, "the Git ref moved after the asset existed");
    let delivery =
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .unwrap();
    let asset = &delivery.assets().assets()[0];
    // §20: the delivered URL ends with the frozen presentation filename, and the
    // object key it is served from carries the same segment.
    assert_eq!(asset.public_filename().unwrap().as_str(), "asset.png");
    assert!(asset.object_key().as_str().ends_with("/asset.png"));
    assert!(asset.public_url().as_str().ends_with("/asset.png"));
    eprintln!(
        "live asset: key={} url={}",
        asset.object_key().as_str(),
        asset.public_url().as_str()
    );

    // Publishing the very same delivery again is a no-op for both targets: the
    // bucket already serves bytes that hash to the frozen identity, so nothing is
    // uploaded a second time and the ref does not move.
    let second = GitPublicationApplication::prepare_and_publish(
        &projection,
        &snapshot,
        &live_public_scope(),
        &config(),
        &repository.local,
        target_id,
        target,
        &metadata,
        &repository.store,
        &runs,
        &deliveries,
        &asset_target,
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut publish_ids,
        &mut observation_ids,
    )
    .unwrap();

    let execution = second.workflow();
    assert!(execution.is_satisfied(), "the rerun must be satisfied");
    assert_eq!(
        execution.assets().published(),
        0,
        "an object the bucket already serves must never be uploaded again"
    );
    assert_eq!(execution.assets().verified(), 1);
    assert_eq!(repository.remote_head(), published, "the ref must not move");

    // The audit trail is durable and complete, exactly as with the native target:
    // every asset inspection in both runs is recorded against its publish run.
    let saved =
        AssetObservationStore::list_for_publish_run(&asset_observations, first.publish_run_id())
            .unwrap();
    // An object that had to be created is observed before and after the upload; an
    // object the bucket already served is observed once.
    let expected = if first_published == 1 { 2 } else { 1 };
    assert_eq!(saved.len(), expected, "the first run's audit rows");
    let saved =
        AssetObservationStore::list_for_publish_run(&asset_observations, second.publish_run_id())
            .unwrap();
    assert_eq!(saved.len(), 1, "a rerun only inspects the object it reuses");
}

#[test]
fn unusable_delivery_configs_fail_closed() {
    for invalid in [
        "",
        "http://assets.example.com",
        "assets.example.com",
        "https://assets.example.com?x=1",
    ] {
        assert!(
            AssetDeliveryConfig::new(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(AssetDeliveryConfig::new("https://assets.example.com/base/").is_ok());
}

#[test]
fn an_unresolvable_binary_reference_is_refused_before_git_is_touched() {
    let Some(repository) = TestRepository::new() else {
        return;
    };
    let bytes = b"![[asset.png]]\n";
    let identity = repository.store.store(bytes).unwrap();
    let snapshot = Snapshot::new(
        SnapshotId::new(1).unwrap(),
        SystemTime::UNIX_EPOCH,
        SourceId::new("test").unwrap(),
        vec![SnapshotFile::new(
            ContentPath::new("note.md").unwrap(),
            bytes.len() as u64,
            identity,
            None,
        )],
    )
    .unwrap();
    let set = FinalPublicationSet::from_parts_for_test(
        snapshot.id(),
        vec![ContentPath::new("note.md").unwrap()],
        vec![],
    );
    let projection =
        PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap();
    // No asset exists, so the reference is an unresolved local reference and
    // delivery refuses instead of guessing a URL.
    assert!(
        DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &repository.store)
            .is_err()
    );
    assert_eq!(
        repository.tree_paths(&repository.head()),
        "content/old.md\n"
    );
}
