use base64;
use chrono;
use git2;
use id;
use level;
use local::Local;
use proof::{self, Content};
use review;
use serde_yaml;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use trust;
use trust_graph;
use util;
use Result;

pub mod staging;

struct RevisionInfo {
    pub type_: String,
    pub revision: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProjectConfig {
    pub version: u64,
    #[serde(rename = "project-id")]
    pub project_id: String,
    #[serde(rename = "project-trust-root")]
    pub project_trust_root: String,
}

const CREV_DOT_NAME: &str = ".crev";

#[derive(Fail, Debug)]
#[fail(display = "Project config not-initialized. Use `crev init` to generate it.")]
struct ProjectDirNotFound;

fn find_project_root_dir() -> Result<PathBuf> {
    let mut path = PathBuf::from(".").canonicalize()?;
    loop {
        if path.join(CREV_DOT_NAME).is_dir() {
            return Ok(path);
        }
        path = if let Some(parent) = path.parent() {
            parent.to_owned()
        } else {
            return Err(ProjectDirNotFound.into());
        }
    }
}

/// `crev` repository
///
/// This represents the `.crev` directory and all
/// the internals of it.
pub struct Repo {
    // root dir, where `.crev` subdiretory resides
    root_dir: PathBuf,
    // lazily loaded `Staging`
    staging: Option<staging::Staging>,
}

impl Repo {
    pub fn init(path: PathBuf, id_str: String) -> Result<Self> {
        let repo = Self::new(path)?;

        fs::create_dir_all(repo.dot_crev_path())?;

        let config_path = repo.project_config_path();
        if config_path.exists() {
            bail!("`{}` already exists", config_path.display());
        }
        util::store_to_file_with(&config_path, move |w| {
            serde_yaml::to_writer(
                w,
                &ProjectConfig {
                    version: 0,
                    project_id: util::random_id_str(),
                    project_trust_root: id_str.clone(),
                },
            )?;

            Ok(())
        })?;

        Ok(repo)
    }

    pub fn auto_open() -> Result<Self> {
        let root_path = find_project_root_dir()?;
        let res = Self::new(root_path)?;

        if !res.project_config_path().exists() {
            bail!("Project config not-initialized. Use `crev init` to generate it.");
        }

        Ok(res)
    }

    pub fn new(root_dir: PathBuf) -> Result<Self> {
        let root_dir = root_dir.canonicalize()?;
        Ok(Self {
            root_dir,
            staging: None,
        })
    }

    fn project_config_path(&self) -> PathBuf {
        self.dot_crev_path().join("config.yaml")
    }

    fn load_project_config(&self) -> Result<ProjectConfig> {
        let path = self.project_config_path();

        let config_str = util::read_file_to_string(&path)?;

        Ok(serde_yaml::from_str(&config_str)?)
    }

    pub fn dot_crev_path(&self) -> PathBuf {
        self.root_dir.join(CREV_DOT_NAME)
    }

    pub fn staging(&mut self) -> Result<&mut staging::Staging> {
        if self.staging.is_none() {
            self.staging = Some(staging::Staging::open(&self.root_dir)?);
        }
        Ok(self.staging.as_mut().unwrap())
    }

    fn append_proof_at<T: proof::Content>(
        &mut self,
        proof: proof::Serialized<T>,
        rel_store_path: &Path,
    ) -> Result<()> {
        let path = self.dot_crev_path().join(rel_store_path);

        fs::create_dir_all(path.parent().expect("Not a root dir"))?;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .write(true)
            .open(path)?;

        file.write_all(proof.to_string().as_bytes())?;
        file.flush()?;

        Ok(())
    }

    pub fn get_proof_rel_store_path(&self, content: &impl proof::Content) -> PathBuf {
        PathBuf::from("proofs").join(content.rel_project_path())
    }

    pub fn verify(&mut self) -> Result<()> {
        let local = Local::auto_open()?;
        let user_config = local.load_user_config()?;
        let cur_id = user_config.current_id;
        let graph = trust_graph::TrustGraph; /* TODO: calculate trust graph */
        /*
        let user_config = Local::read_unlocked_id
        let trust_graph = Local::calculate_trust_graph_for(&id);
        */

        unimplemented!();
        Ok(())
    }

    fn try_read_git_revision(&self) -> Result<Option<RevisionInfo>> {
        let dot_git_path = self.root_dir.join(".git");
        if !dot_git_path.exists() {
            return Ok(None);
        }
        let git_repo = git2::Repository::open(&self.root_dir)?;

        if git_repo.state() != git2::RepositoryState::Clean {
            bail!("Git repository is not in a clean state");
        }
        let mut status_opts = git2::StatusOptions::new();
        status_opts.include_untracked(false);
        if git_repo
            .statuses(Some(&mut status_opts))?
            .iter()
            .any(|entry| {
                if entry.status() != git2::Status::CURRENT {
                    eprintln!("{}", entry.path().unwrap());
                    true
                } else {
                    false
                }
            }) {
            bail!("Git repository is not in a clean state");
        }
        let head = git_repo.head()?;
        let rev = head
            .resolve()?
            .target()
            .ok_or_else(|| format_err!("HEAD target does not resolve to oid"))?
            .to_string();
        Ok(Some(RevisionInfo {
            type_: "git".into(),
            revision: rev,
        }))
    }

    fn read_revision(&self) -> Result<RevisionInfo> {
        if let Some(info) = self.try_read_git_revision()? {
            return Ok(info);
        }
        bail!("Couldn't identify revision info");
    }

    pub fn commit(&mut self) -> Result<()> {
        if self.staging()?.is_empty() {
            bail!("No reviews to commit. Use `add` first.");
        }
        let passphrase = util::read_passphrase()?;
        let local = Local::auto_open()?;
        let id = local.read_unlocked_id(&passphrase)?;
        let pub_id = id.to_pubid();
        let project_config = self.load_project_config()?;
        let revision = self.read_revision()?;
        self.staging()?.enforce_current()?;
        let files = self.staging()?.to_review_files();

        let review = review::ReviewBuilder::default()
            .from(id.pub_key_as_base64())
            .from_url(id.url().into())
            .from_type(id.type_as_string())
            .revision(revision.revision)
            .revision_type(revision.type_)
            .project_id(project_config.project_id)
            .comment(Some("".into()))
            .thoroughness(level::Level::Low)
            .understanding(level::Level::Low)
            .trust(level::Level::Low)
            .files(files)
            .build()
            .map_err(|e| format_err!("{}", e))?;

        let review = util::edit_proof_content_iteractively(&review)?;

        let proof = review.sign(&id)?;

        let rel_store_path = self.get_proof_rel_store_path(&review);

        println!("{}", proof.clone());
        self.append_proof_at(proof.clone(), &rel_store_path)?;
        eprintln!(
            "Proof written to: {}",
            PathBuf::from(".crev").join(rel_store_path).display()
        );
        let local = Local::auto_open()?;
        local.append_proof(&proof, &review);
        eprintln!("Proof added to your store");
        self.staging()?.wipe()?;
        Ok(())
    }

    pub fn status(&mut self) -> Result<()> {
        let staging = self.staging()?;
        for (k, v) in staging.entries.iter() {
            println!("{}", k.display());
        }

        Ok(())
    }

    pub fn add(&mut self, file_paths: Vec<PathBuf>) -> Result<()> {
        let mut staging = self.staging()?;
        for path in file_paths {
            staging.insert(&path);
        }
        staging.save()?;

        Ok(())
    }

    pub fn remove(&mut self, file_paths: Vec<PathBuf>) -> Result<()> {
        let mut staging = self.staging()?;
        for path in file_paths {
            staging.remove(&path);
        }
        staging.save()?;

        Ok(())
    }
}
