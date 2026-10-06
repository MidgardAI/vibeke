#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| vk_fuzz::targets::kitty_probe(data));
