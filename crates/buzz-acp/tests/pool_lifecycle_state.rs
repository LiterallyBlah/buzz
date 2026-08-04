// Compile and run the lifecycle state-machine contract as an integration target.
//
// `pool_lifecycle` names the terminal-auth taxonomy, so that module is pulled
// in here too. It has no other in-crate dependencies, which is what keeps this
// standalone compilation possible.
#[allow(dead_code)]
#[path = "../src/terminal_auth.rs"]
mod terminal_auth;

#[allow(dead_code)]
#[path = "../src/pool_lifecycle.rs"]
mod pool_lifecycle;
