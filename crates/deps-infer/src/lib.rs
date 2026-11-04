pub mod c_include_parser;
pub mod gcc_depfile;
mod gcc_depfile_parser;
mod gcc_include_parser;

pub use gcc_include_parser::parse_include_dirs;
