const ABI_VERSION: u32 = 2;
const GAME_API_SCHEMA_VERSION: u32 = 4;
const CODEC_VERSION: u32 = 1;
const TICK_INPUT_TAG: u32 = 1;
const TICK_RESULT_TAG: u32 = 2;
const HEADER_LEN: i32 = 20;
const EMPTY_TICK_RESULT_LEN: i32 = 28;

export function alloc(len: i32): i32 {
  return changetype<i32>(heap.alloc(len));
}

export function free(ptr: i32, _len: i32): void {
  heap.free(changetype<usize>(ptr));
}

export function tick(input_ptr: i32, input_len: i32, output_ptr: i32, output_len: i32): i32 {
  if (!validTickInputHeader(input_ptr, input_len)) return -1;
  if (output_len < EMPTY_TICK_RESULT_LEN) return -2;

  store<u32>(output_ptr, ABI_VERSION);
  store<u32>(output_ptr + 4, GAME_API_SCHEMA_VERSION);
  store<u32>(output_ptr + 8, CODEC_VERSION);
  store<u32>(output_ptr + 12, TICK_RESULT_TAG);
  store<u32>(output_ptr + 16, 8);
  store<u32>(output_ptr + 20, 0); // commands
  store<u32>(output_ptr + 24, 0); // messages
  return EMPTY_TICK_RESULT_LEN;
}

function validTickInputHeader(input_ptr: i32, input_len: i32): bool {
  if (input_len < HEADER_LEN) return false;
  return load<u32>(input_ptr) == ABI_VERSION
    && load<u32>(input_ptr + 4) == GAME_API_SCHEMA_VERSION
    && load<u32>(input_ptr + 8) == CODEC_VERSION
    && load<u32>(input_ptr + 12) == TICK_INPUT_TAG
    && load<u32>(input_ptr + 16) == <u32>(input_len - HEADER_LEN);
}
