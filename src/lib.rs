#![feature(test, str_internals, core_intrinsics)]
extern crate test;

use core::mem;
// strings pulled from rust-lang.org and localizations. Both should be 700b,
// although it won't matter a ton if this varies a bit.
const EN_MEDIUM: &str = "\
    A language empowering everyone to build reliable and efficient software. \
    Rust is blazingly fast and memory-efficient: with no runtime or garbage \
    collector, it can power performance-critical services, run on embedded \
    devices, and easily integrate with other languages.  Rust’s rich type system \
    and ownership model guarantee memory-safety and thread-safety — enabling you \
    to eliminate many classes of bugs at compile-time.  Rust has great \
    documentation, a friendly compiler with useful error messages, and top-notch \
    tooling — an integrated package manager and build tool, smart multi-editor \
    support with auto-completion and type inspections, an auto-formatter, and \
    more. Performance, reliability
";

const ZH_MEDIUM: &str ="\
    讓每個人都能打造出可靠又高效軟體的程式語言Rust不僅速度驚人，而且節省記憶\
    體。由於不需要執行時函式庫或垃圾回收機制，Rust可以加速高效能需求的服務、\
    執行在嵌入式裝置，並且輕鬆地與其他語言整合。Rust 豐富的型別系統與所有權模型\
    確保了記憶體以及執行緒的安全，讓您在編譯時期就能夠解決各式各樣的錯誤。\
    Rust 擁有完整的技術文件、友善的編譯器與清晰的錯誤訊息，還整合了一流的工具 —\
    包含套件管理工具、建構工具、支援多種編輯器的自動補齊、型別檢測、自動格式化\
    程式碼，以及更多等等。讓每個人都能\
";

// Both 30b
const EN_SMALL: &str = "A language empowering everyone";
const ZH_SMALL: &str = "讓每個人都能打造出。";

// libcore's old impl. LLVM autovectorizes this, although the result has a ton
// of code bloat
fn char_count_old(s: &str) -> usize {
    #[inline]
    fn utf8_is_cont_byte(byte: u8) -> bool {
        const CONT_MASK: u8 = 0b0011_1111;
        const TAG_CONT_U8: u8 = 0b1000_0000;
        (byte & !CONT_MASK) == TAG_CONT_U8
    }
    let bytes_len = s.len();
    let mut cont_bytes = 0;
    for &byte in s.as_bytes() {
        //(b as i8) >= -0x40
        cont_bytes += utf8_is_cont_byte(byte) as usize;
    }
    bytes_len - cont_bytes
}

fn iter_ignore(s: &str) -> usize {
    let mut c = 0;
    for _ in s.chars() {
        c += 1;
    }
    c
}

fn manual_utf8_char_width(s: &str) -> usize {
    let s = s.as_bytes();
    let mut c = 0;
    let mut i = 0;
    let l = s.len();
    while i < l {
        let b = s[i];
        if b < 0x80 {
            i += 1;
        } else if b < 0xe0 {
            i += 2;
        } else if b < 0xf0 {
            i += 3;
        } else {
            i += 4;
        }
        c += 1;
    }
    c
}

fn core_utf8_char_width_lut(s: &str) -> usize {
    let s = s.as_bytes();
    let mut c = 0;
    let mut i = 0;
    let l = s.len();
    while i < l {
        // uses a lookup table
        let step = core::str::utf8_char_width(s[i]);
        debug_assert_ne!(step, 0);
        i += step;
        c += 1;
    }
    c
}

fn core_utf8_char_width_lut2(s: &str) -> usize {
    let s = s.as_bytes();
    let mut c = 0;
    let mut i = 0;
    let l = s.len();
    while i < l {
        let b = s[i];
        if b < 192 { i += 1; c += 1; continue; }
        // uses a lookup table
        i += core::str::utf8_char_width(b);
        c += 1;
    }
    c
}

fn core_utf8_char_width_lut3(s: &str) -> usize {
    let s = s.as_bytes();
    let mut c = 0;
    let mut i = 0;
    let l = s.len();
    'outer:
    while i < l {
        let mut b: u8;
        while { b = *unsafe { s.get_unchecked(i) }; b < 192 } {
            c += (b < 0x80) as usize;
            i += 1;
            if i >= l {
                break 'outer;
            }
        }
        // uses a lookup table
        i += core::str::utf8_char_width(b);
        c += 1;
    }
    c
}

fn char_count_swar_usize(s: &str) -> usize {
    fn count_noncontinuation_bytes(s: &[u8]) -> usize {
        let mut c = 0;
        for &byte in s {
            c += (byte as i8 >= -0x40) as usize;
        }
        c
    }
    // so that I can quickly chek perf diff vs
    type Word = usize;

    #[inline]
    fn is_noncontinuation_byte_swar(w: Word) -> Word {
        const LSB: Word = 0x0101_0101_0101_0101u64 as Word;
        // We want a 1 in the LSB of each byte matching `0b10_??_??_??`,
        ((!w >> 7) | (w >> 6)) & LSB
    }

    #[inline]
    fn hsum_bytes_in_word(values: Word) -> usize {
        const LSB_SHORTS: Word = 0x0001_0001_0001_0001_u64 as Word;
        const SKIP_BYTES: Word = 0x00ff_00ff_00ff_00ff_u64 as Word;
        let pair_sum = (values & SKIP_BYTES) + ((values >> 8) & SKIP_BYTES);
        (pair_sum.wrapping_mul(LSB_SHORTS) >> ((mem::size_of::<Word>() - 2) * 8)) as usize
    }

    // Experimentally determined to be the sweet spot. (If you change this you
    // still need to manually change the inner loop -- this is mostly here for
    // clarity).
    const UNROLL: usize = 4;

    // CHUNK_SIZE needs to be:
    // - Less than or equal to 255 (otherwise we'll overflow bytes in `leads`).
    // - A multiple of UNROLL
    // - Relatively cheap to % against (although 192 seems be better on all
    //   benches than 128)
    // - Large enough to reduce the cost of the hsum, which is not the cheapest
    //   thing here.
    const CHUNK_SIZE: usize = 192;

    let (head, body, tail) = unsafe { s.as_bytes().align_to::<Word>() };

    let mut total = count_noncontinuation_bytes(head) + count_noncontinuation_bytes(tail);

    for chunk in body.chunks(CHUNK_SIZE) {
        let mut counts = 0;
        for words in chunk.chunks_exact(UNROLL) {
            counts += is_noncontinuation_byte_swar(words[0]);
            counts += is_noncontinuation_byte_swar(words[1]);
            counts += is_noncontinuation_byte_swar(words[2]);
            counts += is_noncontinuation_byte_swar(words[3]);
        }
        total += hsum_bytes_in_word(counts);
        if (chunk.len() % UNROLL) != 0 {
            let mut counts = 0;
            let end = &chunk[(chunk.len() - (chunk.len() % UNROLL))..];
            for &word in end {
                counts += is_noncontinuation_byte_swar(word);
            }
            total += hsum_bytes_in_word(counts);
            // The break helps LLVM out, but is only correct if:
            const _: [(); 0] = [(); CHUNK_SIZE % UNROLL];
            break;
        }
    }
    total
}

fn char_count_sse2(s: &str) -> usize {
    #[cfg(all(
        target_feature = "sse2",
        any(target_arch = "x86", target_arch = "x86_64")
    ))]
    {
        unsafe { char_count_sse2_impl(s) }
    }
    #[cfg(not(target_feature = "sse2"))]
    {
        char_count_swar_usize(s)
    }
}

#[cfg(all(
    target_feature = "sse2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
unsafe fn char_count_sse2_impl(s: &str) -> usize {
    fn count_noncontinuation_bytes(s: &[u8]) -> usize {
        let mut c = 0;
        for &byte in s {
            c += (byte as i8 >= -0x40) as usize;
        }
        c
    }
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    // const VEC_SIZE: usize = mem::size_of::<__m128i>();
    const UNROLL: usize = 4;
    const CHUNK_SIZE: usize = 192;

    let (head, body, tail) = s.as_bytes().align_to::<__m128i>();

    let mut total = count_noncontinuation_bytes(head) + count_noncontinuation_bytes(tail);
    let cont_test = _mm_set1_epi8(-0x40);
    for chunk in body.chunks(CHUNK_SIZE) {
        let mut counts = _mm_setzero_si128();
        for vecs in chunk.chunks_exact(UNROLL) {
            counts = _mm_sub_epi8(counts, _mm_cmplt_epi8(cont_test, vecs[0]));
            counts = _mm_sub_epi8(counts, _mm_cmplt_epi8(cont_test, vecs[1]));
            counts = _mm_sub_epi8(counts, _mm_cmplt_epi8(cont_test, vecs[2]));
            counts = _mm_sub_epi8(counts, _mm_cmplt_epi8(cont_test, vecs[3]));
        }
        let sums = _mm_sad_epu8(counts, _mm_setzero_si128());
        total += (_mm_extract_epi32(sums, 0) + _mm_extract_epi32(sums, 2)) as usize;

        if (chunk.len() % UNROLL) != 0 {
            let mut counts = _mm_setzero_si128();
            let end = &chunk[(chunk.len() - (chunk.len() % UNROLL))..];
            for &vec in end {
                counts = _mm_sub_epi8(counts, _mm_cmplt_epi8(cont_test, vec));
            }
            let sums = _mm_sad_epu8(counts, _mm_setzero_si128());
            total += (_mm_extract_epi32(sums, 0) + _mm_extract_epi32(sums, 2)) as usize;
            // The break helps LLVM out, but is only correct if:
            const _: [(); 0] = [(); CHUNK_SIZE % UNROLL];
            break;
        }
    }
    total
}


macro_rules! bench_matrix {
    (
        @funcs: $funcs:tt $(,)?
        @inputs: {$($input_mod_name:ident : $exp:expr),+ $(,)?} $(,)?
    ) => {
        $(
            bench_matrix!{
                @inner
                @funcs: $funcs
                @inputs: { $input_mod_name : $exp }
            }
        )*
    };
    (
        @inner
        @funcs: [$($func:ident),+ $(,)?] $(,)?
        @inputs: {$input_mod_name:ident : $exp:expr}
    ) => {
        mod $input_mod_name {
            use super::*;
            // Note: needs to be outside inner $(...)+
            fn bench_input() -> String {
                $exp.into()
            }
            $(
                #[bench]
                fn $func(bencher: &mut test::Bencher) {
                    let mut input: String = bench_input();
                    bencher.bytes = input.len() as u64;
                    bencher.iter(|| {
                        let slice = test::black_box(&mut input);
                        test::black_box(super::$func(slice))
                    });
                }
            )+
        }
    };
}

bench_matrix! {
    @funcs: [
        char_count_old,
        iter_ignore,
        manual_utf8_char_width,
        core_utf8_char_width_lut,
        core_utf8_char_width_lut2,
        core_utf8_char_width_lut3,
        char_count_swar_usize,
        char_count_sse2,
    ],
    @inputs: {
        en_30b: EN_SMALL,
        zh_30b: ZH_SMALL,

        en_600b: EN_MEDIUM,
        zh_600b: ZH_MEDIUM,

        en_5kb: EN_MEDIUM.repeat(8),
        zh_5kb: ZH_MEDIUM.repeat(8),

        en_300kb: EN_MEDIUM.repeat(512),
        zh_300kb: ZH_MEDIUM.repeat(512),

        mixed_6kb: format!(
            "{}{}{}{}",
            EN_SMALL,
            ZH_SMALL,
            EN_MEDIUM,
            ZH_MEDIUM,
        ).repeat(4),
    },
}
