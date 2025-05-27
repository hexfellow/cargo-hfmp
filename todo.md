1. Stop manully making stuff! Use cargo_metadata to get file path, etc. 
   1. So according to https://github.com/rust-lang/cargo/issues/7546, well it is not possible to get OUT_DIR.
   2. Maybe just write a kv string to elf file and read from elf? 
   3. Only stop using `find` when multiple files really becomes a problem.