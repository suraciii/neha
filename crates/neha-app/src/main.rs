//! `neha` runs a Space and talks to its Bots from a terminal.

fn main() -> anyhow::Result<()> {
    neha_app::cli::run()
}
