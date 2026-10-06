#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| vk_fuzz::targets::holder_proto_decode(data));
