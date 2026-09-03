//! Static recompilation of Game Boy code to native x86-64.
//!
//! The pipeline is: discover reachable code in the cartridge, decode it, and
//! translate each basic block into a self-contained run of x86-64 machine code.
//! Nothing here ever interprets an SM83 instruction — the game's own code becomes
//! host code, and the only thing left at runtime is the hardware it talks to.

pub mod abi;
pub mod decode;
pub mod discover;
pub mod exec;
pub mod machine;
pub mod translate;
pub mod x64;

pub use abi::{GbState, JumpEntry};
pub use decode::{decode, Flow, Insn};
pub use discover::{discover, Report};
pub use exec::{BlockKey, CodeCache};
pub use machine::Emu;
pub use translate::{translate_block, Block, Context, Link};
