use crev_data::proof::PackageInfo;
use crev_data::review::Package;
use crev_data::Review;
use crev_data::{Id, Level, PublicId, Rating, TrustLevel, Url, SOURCE_CRATES_IO};
use crev_lib::Local;
use crev_wot::ProofDB;
use crev_wot::TrustSet;
use crev_wot::{PkgVersionReviewId, TrustDistanceParams};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;

pub mod vet;

pub use crev_lib::Error;

pub struct Crevette {
    db: ProofDB,
    trusts: TrustSet,
    min_trust_level: TrustLevel,
    /// Presenve of a git rev makes vargo-vet ignore the review entirely
    include_git_revs: bool,
}

impl Crevette {
    /// Use crev's defaults, and export most reviews.
    ///
    /// Requires a crev Id already set up, and reviews fetched.
    ///
    /// See `cargo crev id new` and `cargo crev repo fetch all`
    pub fn new() -> Result<Self, Error> {
        let local = Local::auto_open()?;
        let db = local.load_db()?;
        Self::new_with_options(
            db,
            &local.get_current_userid()?,
            &TrustDistanceParams::default(),
            TrustLevel::Low,
        )
    }

    /// Export reviews from the given db, if they meet minimum trust level,
    /// based on the `trust_params`, from perspective of the given Id.
    pub fn new_with_options(
        db: ProofDB,
        id: &Id,
        trust_params: &TrustDistanceParams,
        min_trust_level: TrustLevel,
    ) -> Result<Self, Error> {
        let trusts = db.calculate_trust_set(id, trust_params);

        Ok(Self {
            db,
            trusts,
            min_trust_level,
            include_git_revs: false,
        })
    }

    /// Write `audits.toml` to your current crev repository.
    ///
    /// After `cargo crev publish` the audit will be available in your crev-proofs repo.
    pub fn convert_into_repo(&self) -> Result<RepoInfo, Error> {
        let toml = self.convert_to_toml()?;
        let local = Local::auto_open()?;
        let path = local.get_proofs_dir_path()?;
        let audit_path = path.join("audits.toml");
        if let Err(e) = std::fs::write(&audit_path, toml) {
            return Err(Error::FileWrite(e, audit_path));
        }
        local.proof_dir_git_add_path("audits.toml".as_ref())?;
        local.proof_dir_commit("Updated audits.toml")?;

        let mut repo_git_url = Local::url_for_repo_at_path(&path).ok();
        if let Some(u) = &repo_git_url {
            if let Some((host, rest)) = u.strip_prefix("git@").and_then(|u| u.split_once(':')) {
                repo_git_url = Some(format!("https://{host}/{rest}"));
            }
        }

        let (repo_https_url, repo_name) = repo_git_url
            .as_deref()
            .and_then(|u| {
                let u = u.trim_end_matches('/').trim_end_matches(".git");
                if let Some(rest) = u.strip_prefix("https://github.com/") {
                    Some((
                        format!("https://raw.githubusercontent.com/{rest}/HEAD/audits.toml"),
                        rest.split('/').next().unwrap_or_default().into(),
                    ))
                } else {
                    u.strip_prefix("https://gitlab.com/").map(|rest| (
                        format!("https://gitlab.com/{rest}/-/raw/HEAD/audits.toml"),
                        rest.split('/').next().unwrap_or_default().into(),
                    ))
                }
            })
            .unzip();

        Ok(RepoInfo {
            local_path: audit_path,
            repo_git_url,
            repo_https_url,
            repo_name,
        })
    }

    /// Here's your cargo-vet-compatible `audits.toml` file
    pub fn convert_to_toml(&self) -> Result<String, Error> {
        let mut toml = toml_edit::ser::to_string_pretty(&self.convert_to_document()?)
            .map_err(|toml| Error::IO(io::Error::new(io::ErrorKind::Other, toml)))?;

        toml.insert_str(0, &format!("# Automatically generated by https://lib.rs/crevette {} from cargo-crev reviews\n\n", env!("CARGO_PKG_VERSION")));

        Ok(toml)
    }

    #[cfg(feature = "debcargo")]
    pub fn from_debcargo_repo(temp_dir_path: &std::path::Path) -> Result<String, Error> {
        let _ = std::fs::create_dir_all(&temp_dir_path);

        let deb_err = |e: index_debcargo::Error| Error::ErrorIteratingLocalProofStore(Box::new((temp_dir_path.into(), e.to_string())));
        let mut d = index_debcargo::Index::new(temp_dir_path).map_err(deb_err)?;

        let sources_file = temp_dir_path.join("Sources.gz");
        if !sources_file.exists() {
            let sources_file_tmp = temp_dir_path.join("Sources.gz.tmp");
            let sources_url = "https://deb.debian.org/debian/dists/stable/main/source/Sources.gz";
            let mut out = std::fs::File::create(&sources_file_tmp)?;
            let dl_err = |e| Error::IO(io::Error::new(io::ErrorKind::Other, format!("Can't download {sources_url}: {e}")));
            let mut response = match reqwest::blocking::get(sources_url) {
                Ok(r) => r,
                Err(e) => return Err(dl_err(e)),
            };
            response.copy_to(&mut out).map_err(dl_err)?;
            std::fs::rename(&sources_file_tmp, &sources_file)?;
        }
        let sources_gzipped = std::fs::File::open(&sources_file)?;
        let sources = flate2::read::GzDecoder::new(sources_gzipped);

        d.add_distro_source("stable", io::BufReader::new(sources)).map_err(deb_err)?;

        let debs = d.list_all().map_err(deb_err)?;

        let mut audits = BTreeMap::new();
        let mut seen = std::collections::HashSet::new();
        for d in debs {
            let mut who = vec![];
            seen.clear();
            if let Some(email) = d.maintainer_email {
                who.push(format!("\"{}\" <{email}>", d.maintainer_name.as_deref().unwrap_or_default()));
                seen.insert(email);
                if let Some(name) = d.maintainer_name {
                    seen.insert(name);
                }
            }
            for a in &d.uploaders {
                let a = cargo_author::Author::new(a);
                if let Some(email) = a.email {
                    let uploader = format!("\"{}\" <{email}>", a.name.as_deref().unwrap_or_default());
                    if let Some(name) = a.name {
                        if !seen.insert(name) { continue; }
                    }
                    if !seen.insert(email) { continue; }
                    who.push(uploader);
                }
            }

            let distros = d.distros.join(", ");
            let distros = if distros.is_empty() { "unreleased" } else { &distros };

            audits.entry(d.name).or_insert_with(Vec::new).push(vet::AuditEntry {
                criteria: vec!["safe-to-run", "safe-to-deploy"],
                aggregated_from: vec![index_debcargo::DEBCARGO_CONF_REPO_URL.to_string()],
                notes: Some(format!("Packaged for Debian ({distros}). Changelog:\n{}", d.changelog)),
                delta: None,
                version: Some(d.version),
                violation: None,
                who: vet::StringOrVec::Vec(who),
            });
        }


        let audits = vet::AuditsFile {
            criteria: Default::default(),
            audits,
        };

        let mut toml = toml_edit::ser::to_string_pretty(&audits)
            .map_err(|toml| Error::IO(io::Error::new(io::ErrorKind::Other, toml)))?;

        toml.insert_str(0, &format!("# Automatically generated by https://lib.rs/crevette {} from debcargo-conf repo\n\n", env!("CARGO_PKG_VERSION")));

        Ok(toml)
    }

    #[cfg(feature = "guix")]
    pub fn from_guix_repo(temp_dir_path: &std::path::Path) -> Result<String, Error> {
        let _ = std::fs::create_dir_all(&temp_dir_path);

        let g_err = |e: index_guix::Error| Error::ErrorIteratingLocalProofStore(Box::new((temp_dir_path.into(), e.to_string())));
        let g = index_guix::Index::new(temp_dir_path).map_err(g_err)?;

        let all = g.list_all().map_err(g_err)?;

        let mut audits = BTreeMap::new();
        for (category, packages) in all {
            for p in packages {
                audits.entry(p.name).or_insert_with(Vec::new).push(vet::AuditEntry {
                    criteria: vec!["safe-to-run"],
                    aggregated_from: vec![index_guix::GUIX_REPO_URL.to_string()],
                    notes: Some(format!("Packaged for Guix ({category})")),
                    delta: None,
                    version: Some(p.version),
                    violation: None,
                    who: vet::StringOrVec::Vec(vec![]),
                });
            }
        }

        let audits = vet::AuditsFile {
            criteria: Default::default(),
            audits,
        };

        let mut toml = toml_edit::ser::to_string_pretty(&audits)
            .map_err(|toml| Error::IO(io::Error::new(io::ErrorKind::Other, toml)))?;

        toml.insert_str(0, &format!("# Automatically generated by https://lib.rs/crevette {} from guix repo\n\n", env!("CARGO_PKG_VERSION")));

        Ok(toml)
    }

    pub fn convert_to_document(&self) -> Result<vet::AuditsFile, Error> {
        // audits BTreeMap will sort reviews by crate
        let mut all = HashMap::new();

        for r in self.db.get_pkg_reviews_for_source(SOURCE_CRATES_IO) {
            let Some(review) = r.review() else { continue };

            let trust = self.trusts.get_effective_trust_level(&r.common.from.id);
            if trust < self.min_trust_level {
                continue;
            }

            let review_quality_score = level_as_score(review.thoroughness) + level_as_score(review.understanding);
            all.entry(&r.package.id.id).or_insert_with(Vec::new).push((trust, review_quality_score, r));
        }

        let mut audits = BTreeMap::default();
        for reviews_for_crate in all.values_mut() {
            reviews_for_crate.sort_by(|(a_trust, q_a, a), (b_trust, q_b, b)| {
                b.package.id.version.cmp(&a.package.id.version)
                    .then(b_trust.cmp(a_trust))
                    .then(q_b.cmp(q_a))
                    .then(b.common.date.cmp(&a.common.date))
            });

            let mut last_review = None;
            for &(trust, review_quality_score, r) in &*reviews_for_crate {
                let Some(review) = r.review() else { continue };

                let pub_id = &r.common.from;

                let violation = review.rating == Rating::Negative;
                let criteria = if violation {
                    let severity = r.issues.iter().map(|i| i.severity)
                        .chain(r.advisories.iter().map(|a| a.severity))
                        .max().unwrap_or(Level::Medium);
                    match severity {
                        Level::None => vec!["level-none"], // not sure if that makes sense
                        Level::Low => vec!["level-low"],
                        Level::Medium => vec!["safe-to-deploy"],
                        Level::High => vec!["safe-to-run", "safe-to-deploy"],
                    }
                } else {
                    let min_score = match trust {
                        TrustLevel::Distrust | TrustLevel::None => continue,
                        TrustLevel::Low => level_as_score(Level::High),
                        TrustLevel::Medium => level_as_score(Level::Medium),
                        TrustLevel::High => level_as_score(Level::Low),
                    } + match review.rating {
                        Rating::Negative => level_as_score(Level::None),
                        Rating::Neutral => level_as_score(Level::Medium),
                        Rating::Positive => level_as_score(Level::Low),
                        Rating::Strong => level_as_score(Level::None),
                    };

                    if review_quality_score < min_score {
                        continue;
                    }

                    // Avoid exporting pareto-worse reviews
                    if let Some((l_review_quality_score, l_trust, ref l_version)) = last_review {
                        if l_review_quality_score >= review_quality_score {
                            if *l_version > r.package.id.version && l_trust >= trust {
                                continue;
                            }
                            if *l_version >= r.package.id.version && l_trust > trust {
                                continue;
                            }
                        }
                    }

                    criteria_for_non_negative_review(trust, r, review, review_quality_score)
                };

                let public_url = self.db.lookup_url(&pub_id.id).verified();
                let base_url = public_url
                    .map(|u| format!("{}#{}", u.url, pub_id.id))
                    .unwrap_or_else(|| format!("crev:user/{}", pub_id.id));

                if violation && public_url.map_or(false, |u| u.url.contains("MaulingM")) {
                    continue;
                }

                let (version, delta) = if violation {
                    (None, None)
                } else if let Some(base) = &r.diff_base {
                    (
                        None,
                        Some(format!(
                            "{} -> {}",
                            self.vet_version(base),
                            self.vet_version(&r.package)
                        )),
                    )
                } else {
                    (Some(self.vet_version(&r.package)), None)
                };

                let Some(digest) = self
                    .db
                    .get_proof_digest_by_pkg_review_id(&PkgVersionReviewId::from(r))
                else {
                    continue;
                };

                let mut notes = Some(&r.comment)
                    .filter(|c| !c.trim_start().is_empty())
                    .cloned();

                let mut out = String::new();
                for adv in &r.advisories {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("severity: {}\n", adv.severity));
                    if !adv.ids.is_empty() {
                        out.push_str("id: ");
                        out.push_str(&adv.ids.join(", "));
                        out.push('\n');
                    }
                    if !adv.comment.is_empty() {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&adv.comment);
                    }
                }

                for issue in &r.issues {
                    out.push_str(&format!("severity: {}\nid: {}\n", issue.severity, issue.id));
                    if !issue.comment.is_empty() {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&issue.comment);
                    }
                }

                if !out.is_empty() {
                    match notes.as_mut() {
                        None => { notes = Some(out); },
                        Some(notes) => {
                            notes.push('\n');
                            notes.push_str(&out);
                        }
                    }
                }

                audits
                    .entry(r.package.id.id.name.clone())
                    .or_insert_with(Vec::new)
                    .push(vet::AuditEntry {
                        violation: violation.then(|| format!("={}", r.package.id.version)),
                        who: vet::StringOrVec::String(author_from_id(pub_id, public_url)),
                        criteria,
                        notes: notes.or_else(|| violation.then(|| format!("<https://lib.rs/crates/{}/audit>", r.package.id.id.name))),
                        aggregated_from: vec![
                            base_url.clone(),
                            format!("crev:review/{}", digest.to_base64()),
                        ],
                        version,
                        delta,
                    });
                // Candidate for being a better review than the next one
                last_review = (review.rating > Rating::Neutral
                    && r.diff_base.is_none()
                    && r.package.id.version.pre.is_empty())
                .then_some((review_quality_score, trust, r.package.id.version.clone()));
            }
        }

        Ok(vet::AuditsFile {
            criteria: standard_criteria(),
            audits,
        })
    }

    fn vet_version(&self, pkg: &PackageInfo) -> String {
        if self.include_git_revs && pkg.revision_type == "git" && !pkg.revision.is_empty() {
            format!("{}@git:{}", pkg.id.version, pkg.revision)
        } else {
            pkg.id.version.to_string()
        }
    }
}

fn criteria_for_non_negative_review(trust: TrustLevel, r: &Package, review: &Review, review_quality_score: u32) -> Vec<&'static str> {
    let safe_to_run = trust >= TrustLevel::Medium
        && match review.rating {
            Rating::Negative => false,
            Rating::Neutral => {
                review_quality_score
                    >= level_as_score(Level::Medium) + level_as_score(Level::Medium)
            }
            Rating::Positive => {
                review_quality_score >= level_as_score(Level::Medium) + level_as_score(Level::Low)
            }
            Rating::Strong => {
                review_quality_score >= level_as_score(Level::Low) + level_as_score(Level::Low)
            }
        };
    let safe_to_deploy = safe_to_run
        && review.understanding >= Level::Medium
        && match review.rating {
            Rating::Negative => false,
            Rating::Neutral => review.thoroughness >= Level::High,
            Rating::Positive => review.thoroughness >= Level::Medium,
            Rating::Strong => review.thoroughness >= Level::Low,
        };
    let criterion = match review.rating {
        Rating::Negative => "negative",
        Rating::Neutral => "neutral",
        Rating::Positive => "positive",
        Rating::Strong => "strong",
    };
    let trust_criterion = match trust {
        TrustLevel::Distrust | TrustLevel::None => unreachable!(),
        TrustLevel::Low => "trust-low",
        TrustLevel::Medium => "trust-medium",
        TrustLevel::High => "trust-high",
    };
    let level = if review_quality_score >= level_as_score(Level::High) * 2 {
        "level-high"
    } else if review_quality_score >= level_as_score(Level::Medium) * 2 {
        "level-medium"
    } else if review_quality_score >= level_as_score(Level::Low) * 2 {
        "level-low"
    } else {
        "level-none"
    };
    let mut criteria = vec![criterion, level, trust_criterion];
    if safe_to_deploy {
        criteria.push("safe-to-deploy");
    }
    if safe_to_run {
        criteria.push("safe-to-run");
    }
    if r.flags.unmaintained {
        criteria.push("unmaintained");
    }
    criteria
}

/// Result of `convert_to_repo`
pub struct RepoInfo {
    pub local_path: PathBuf,
    pub repo_git_url: Option<String>,
    pub repo_https_url: Option<String>,
    pub repo_name: Option<String>,
}

fn author_from_id(pub_id: &PublicId, verified_url: Option<&Url>) -> String {
    if let Some(url) = verified_url.map(|u| u.url.as_str()) {
        let url = url.strip_suffix("/crev-proofs").unwrap_or(url);
        let username = [
            "https://github.com/",
            "https://gitlab.com/",
            "https://git.sr.ht/~",
        ]
        .iter()
        .find_map(|pref| url.strip_prefix(pref))
        .and_then(|rest| rest.split('/').next());
        if let Some(username) = username {
            return format!("\"{username}\" ({url})");
        }
        if let Some(host) = url
            .strip_prefix("https://")
            .and_then(|rest| rest.split('/').next())
        {
            return format!("\"{host}\" ({url})");
        }
        url.to_string()
    } else {
        format!("https://web.crev.dev/rust-reviews/reviewer/{}", pub_id.id)
    }
}

fn level_as_score(level: Level) -> u32 {
    match level {
        Level::None => 0,
        Level::Low => 1,
        Level::Medium => 3,
        Level::High => 7,
    }
}

fn standard_criteria() -> BTreeMap<&'static str, vet::CriteriaEntry> {
    let crev_criteria_url = vec!["https://github.com/crev-dev".into()];
    [
        ("trust-high", vet::CriteriaEntry {
            description: Some("Author of this review is well known and trusted by the publisher of this audit repository. This means 'at least this much', so higher levels imply all lower levels"),
            implies: vec!["trust-medium"],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("trust-medium", vet::CriteriaEntry {
            description: Some("Author of this review is somewhat known and trusted by the publisher of this audit repository"),
            implies: vec!["trust-low"],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("trust-low", vet::CriteriaEntry {
            description: Some("Author of this review is not well known, or not trusted much, by the publisher of this audit repository"),
            implies: vec![],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("strong", vet::CriteriaEntry {
            description: Some("Strong endorsement. It implies a positive rating"),
            implies: vec!["positive"],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("positive", vet::CriteriaEntry {
            description: Some("Positive review rating"),
            implies: vec![],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("neutral", vet::CriteriaEntry {
            description: Some("There is no rating either way. Check the comments for reports of issues"),
            implies: vec![],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("level-high", vet::CriteriaEntry {
            description: Some("The code has been thoroughly reviewed and/or with high understanding. This means 'at least this much' so higher levels imply all lower levels"),
            implies: vec!["level-medium"],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("level-medium", vet::CriteriaEntry {
            description: Some("The code has been reviewed with average thoroughness or understanding. This means 'at least this much' so higher levels imply all lower levels"),
            implies: vec!["level-low"],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("level-low", vet::CriteriaEntry {
            description: Some("The code has been only checked at a glance and/or with low understanding. This means 'at least this much' so higher levels imply all lower levels"),
            implies: vec!["level-none"],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("level-none", vet::CriteriaEntry {
            description: Some("The code hasn't been reviewed or hasn't been understood"),
            implies: vec![],
            aggregated_from: crev_criteria_url.clone(),
        }),
        ("unmaintained", vet::CriteriaEntry {
            description: Some("The package has been flagged as unmaintained"),
            implies: vec![],
            aggregated_from: crev_criteria_url.clone(),
        }),
    ].into_iter().collect()
}
