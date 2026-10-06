use camino::Utf8Path;
use fervor_domain::environment::{EnvironmentSpec, ResolvedEnvironment, ResolvedPackage};
use rattler_conda_types::Subdir;
use rattler_repodata_gateway::Gateway;
use rattler_solve::{SolveError, SolverImpl, SolverTask, resolvo};

use crate::client::CondaClient;
use crate::error::ResolveError;

/// Solves environments with rattler's repodata gateway and the resolvo solver.
///
/// Repodata is cached under `<cache root>/repodata`.
#[derive(Clone)]
pub struct RattlerResolver {
    gateway: Gateway,
}

impl RattlerResolver {
    pub fn new(cache_root: &Utf8Path, client: CondaClient) -> Self {
        let gateway = Gateway::builder()
            .with_cache_dir(cache_root.join("repodata").into_std_path_buf())
            .with_client(client.http().clone())
            .finish();
        Self { gateway }
    }

    /// Solves `spec` into an exact package set.
    pub async fn resolve(&self, spec: &EnvironmentSpec) -> Result<ResolvedEnvironment, ResolveError> {
        let platform = spec.platform();
        let specs = spec.solver_requirements();
        let repodata = self
            .gateway
            .query(
                spec.channels().iter().cloned(),
                [platform.subdir(), Subdir::NoArch],
                specs.clone(),
            )
            .recursive(true)
            .execute()
            .await
            .map_err(|source| ResolveError::infrastructure("failed to load repodata", source))?;

        let virtual_packages = spec.virtual_packages();
        let solved = tokio::task::spawn_blocking(move || {
            let task = SolverTask {
                virtual_packages,
                specs,
                ..SolverTask::from_iter(repodata.iter().map(|repodata| repodata.iter()))
            };
            resolvo::Solver.solve(task)
        })
        .await
        .map_err(|source| ResolveError::infrastructure("the solver task panicked", source))?;
        let solution = solved.map_err(|error| match error {
            SolveError::Unsolvable(messages) => ResolveError::Unsolvable(messages.join("\n")),
            other => ResolveError::infrastructure("the solver failed", other),
        })?;

        let packages = solution
            .records
            .into_iter()
            .map(ResolvedPackage::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ResolvedEnvironment::new(platform, packages)?)
    }
}
