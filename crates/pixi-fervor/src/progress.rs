use fervor_app::BuildEvent;

/// Prints build progress to stderr.
pub struct StderrProgress;

impl StderrProgress {
    pub fn report(event: BuildEvent<'_>) {
        match event {
            BuildEvent::Resolved(env) => eprintln!(
                "resolved {} packages for {} (glibc from {})",
                env.packages().len(),
                env.platform(),
                env.sysroot().display_name()
            ),
            BuildEvent::Planned(plan) => eprintln!("planned {} layers + boot layer", plan.layers().len()),
            BuildEvent::LayerReused(layer) => eprintln!("  cached  {:>10}  {}", layer.size.to_string(), layer.label),
            BuildEvent::LayerBuilt { layer, elapsed } => {
                eprintln!("  built   {:>10}  {} ({:.1}s)", layer.size.to_string(), layer.label, elapsed.as_secs_f64())
            }
        }
    }
}
