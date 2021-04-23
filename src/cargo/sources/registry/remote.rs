use crate::core::{GitReference, PackageId, SourceId};
use crate::sources::git;
use crate::sources::registry::download;
use crate::sources::registry::MaybeLock;
use crate::sources::registry::{RegistryConfig, RegistryData};
use crate::util::errors::{CargoResult, CargoResultExt};
use crate::util::interning::InternedString;
use crate::util::paths;
use crate::util::{Config, Filesystem};
use lazycell::LazyCell;
use log::{debug, trace};
use std::cell::{Cell, Ref, RefCell};
use std::fs::File;
use std::mem;
use std::path::Path;
use std::str;

pub struct RemoteRegistry<'cfg> {
    index_path: Filesystem,
    cache_path: Filesystem,
    source_id: SourceId,
    index_git_ref: GitReference,
    config: &'cfg Config,
    tree: RefCell<Option<git2::Tree<'static>>>,
    repo: LazyCell<git2::Repository>,
    head: Cell<Option<git2::Oid>>,
    current_sha: Cell<Option<InternedString>>,
}

impl<'cfg> RemoteRegistry<'cfg> {
    pub fn new(source_id: SourceId, config: &'cfg Config, name: &str) -> RemoteRegistry<'cfg> {
        RemoteRegistry {
            index_path: config.registry_index_path().join(name),
            cache_path: config.registry_cache_path().join(name),
            source_id,
            config,
            // TODO: we should probably make this configurable
            index_git_ref: GitReference::DefaultBranch,
            tree: RefCell::new(None),
            repo: LazyCell::new(),
            head: Cell::new(None),
            current_sha: Cell::new(None),
        }
    }

    fn repo(&self) -> CargoResult<&git2::Repository> {
        self.repo.try_borrow_with(|| {
            let path = self.config.assert_package_cache_locked(&self.index_path);

            // Fast path without a lock
            if let Ok(repo) = git2::Repository::open(&path) {
                trace!("opened a repo without a lock");
                return Ok(repo);
            }

            // Ok, now we need to lock and try the whole thing over again.
            trace!("acquiring registry index lock");
            match git2::Repository::open(&path) {
                Ok(repo) => Ok(repo),
                Err(_) => {
                    drop(paths::remove_dir_all(&path));
                    paths::create_dir_all(&path)?;

                    // Note that we'd actually prefer to use a bare repository
                    // here as we're not actually going to check anything out.
                    // All versions of Cargo, though, share the same CARGO_HOME,
                    // so for compatibility with older Cargo which *does* do
                    // checkouts we make sure to initialize a new full
                    // repository (not a bare one).
                    //
                    // We should change this to `init_bare` whenever we feel
                    // like enough time has passed or if we change the directory
                    // that the folder is located in, such as by changing the
                    // hash at the end of the directory.
                    //
                    // Note that in the meantime we also skip `init.templatedir`
                    // as it can be misconfigured sometimes or otherwise add
                    // things that we don't want.
                    let mut opts = git2::RepositoryInitOptions::new();
                    opts.external_template(false);
                    Ok(git2::Repository::init_opts(&path, &opts)
                        .chain_err(|| "failed to initialize index git repository")?)
                }
            }
        })
    }

    fn head(&self) -> CargoResult<git2::Oid> {
        if self.head.get().is_none() {
            let repo = self.repo()?;
            let oid = self.index_git_ref.resolve(repo, None)?;
            self.head.set(Some(oid));
        }
        Ok(self.head.get().unwrap())
    }

    fn tree(&self) -> CargoResult<Ref<'_, git2::Tree<'_>>> {
        {
            let tree = self.tree.borrow();
            if tree.is_some() {
                return Ok(Ref::map(tree, |s| s.as_ref().unwrap()));
            }
        }
        let repo = self.repo()?;
        let commit = repo.find_commit(self.head()?)?;
        let tree = commit.tree()?;

        // Unfortunately in libgit2 the tree objects look like they've got a
        // reference to the repository object which means that a tree cannot
        // outlive the repository that it came from. Here we want to cache this
        // tree, though, so to accomplish this we transmute it to a static
        // lifetime.
        //
        // Note that we don't actually hand out the static lifetime, instead we
        // only return a scoped one from this function. Additionally the repo
        // we loaded from (above) lives as long as this object
        // (`RemoteRegistry`) so we then just need to ensure that the tree is
        // destroyed first in the destructor, hence the destructor on
        // `RemoteRegistry` below.
        let tree = unsafe { mem::transmute::<git2::Tree<'_>, git2::Tree<'static>>(tree) };
        *self.tree.borrow_mut() = Some(tree);
        Ok(Ref::map(self.tree.borrow(), |s| s.as_ref().unwrap()))
    }
}

const LAST_UPDATED_FILE: &str = ".last-updated";

impl<'cfg> RegistryData for RemoteRegistry<'cfg> {
    fn prepare(&self) -> CargoResult<()> {
        self.repo()?; // create intermediate dirs and initialize the repo
        Ok(())
    }

    fn index_path(&self) -> &Filesystem {
        &self.index_path
    }

    fn assert_index_locked<'a>(&self, path: &'a Filesystem) -> &'a Path {
        self.config.assert_package_cache_locked(path)
    }

    fn current_version(&self) -> Option<InternedString> {
        if let Some(sha) = self.current_sha.get() {
            return Some(sha);
        }
        let sha = InternedString::new(&self.head().ok()?.to_string());
        self.current_sha.set(Some(sha));
        Some(sha)
    }

    fn load(
        &mut self,
        _root: &Path,
        path: &Path,
        data: &mut dyn FnMut(&[u8]) -> CargoResult<()>,
    ) -> CargoResult<()> {
        // Note that the index calls this method and the filesystem is locked
        // in the index, so we don't need to worry about an `update_index`
        // happening in a different process.
        let repo = self.repo()?;
        let tree = self.tree()?;
        let entry = tree.get_path(path)?;
        let object = entry.to_object(repo)?;
        let blob = match object.as_blob() {
            Some(blob) => blob,
            None => anyhow::bail!("path `{}` is not a blob in the git repo", path.display()),
        };
        data(blob.content())
    }

    fn config(&mut self) -> CargoResult<Option<RegistryConfig>> {
        debug!("loading config");
        self.prepare()?;
        self.config.assert_package_cache_locked(&self.index_path);
        let mut config = None;
        self.load(Path::new(""), Path::new("config.json"), &mut |json| {
            config = Some(serde_json::from_slice(json)?);
            Ok(())
        })?;
        trace!("config loaded");
        Ok(config)
    }

    fn update_index(&mut self) -> CargoResult<()> {
        if self.config.offline() {
            return Ok(());
        }
        if self.config.cli_unstable().no_index_update {
            return Ok(());
        }
        // Make sure the index is only updated once per session since it is an
        // expensive operation. This generally only happens when the resolver
        // is run multiple times, such as during `cargo publish`.
        if self.config.updated_sources().contains(&self.source_id) {
            return Ok(());
        }

        debug!("updating the index");

        // Ensure that we'll actually be able to acquire an HTTP handle later on
        // once we start trying to download crates. This will weed out any
        // problems with `.cargo/config` configuration related to HTTP.
        //
        // This way if there's a problem the error gets printed before we even
        // hit the index, which may not actually read this configuration.
        self.config.http()?;

        self.prepare()?;
        self.head.set(None);
        *self.tree.borrow_mut() = None;
        self.current_sha.set(None);
        let path = self.config.assert_package_cache_locked(&self.index_path);
        self.config
            .shell()
            .status("Updating", self.source_id.display_index())?;

        // Fetch the latest version of our `index_git_ref` into the index
        // checkout.
        let url = self.source_id.url();
        let repo = self.repo.borrow_mut().unwrap();
        git::fetch(repo, url.as_str(), &self.index_git_ref, self.config)
            .chain_err(|| format!("failed to fetch `{}`", url))?;
        self.config.updated_sources().insert(self.source_id);

        // Create a dummy file to record the mtime for when we updated the
        // index.
        paths::create(&path.join(LAST_UPDATED_FILE))?;

        Ok(())
    }

    fn download(&mut self, pkg: PackageId, checksum: &str) -> CargoResult<MaybeLock> {
        let filename = download::filename(pkg);
        let path = self.cache_path.join(&filename);
        let path = self.config.assert_package_cache_locked(&path);
        download::download(self, &path, pkg, checksum)
    }

    fn finish_download(
        &mut self,
        pkg: PackageId,
        checksum: &str,
        data: &[u8],
    ) -> CargoResult<File> {
        download::finish_download(&self.cache_path, &self.config, pkg, checksum, data)
    }

    fn is_crate_downloaded(&self, pkg: PackageId) -> bool {
        download::is_crate_downloaded(&self.cache_path, &self.config, pkg)
    }
}

impl<'cfg> Drop for RemoteRegistry<'cfg> {
    fn drop(&mut self) {
        // Just be sure to drop this before our other fields
        self.tree.borrow_mut().take();
    }
}
