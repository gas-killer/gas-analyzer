/* gk-guest-crt linker script — one layout for every gkvm guest, whatever
 * compiled it (riscv64 gcc, clang, or rustc's lld).
 *
 * The SP1 v6 loader's rules, which this script exists to satisfy:
 *   - every PT_LOAD segment lives at or above STACK_TOP = 0x78000000
 *     (the stack grows DOWN from 0x78000000 toward zero);
 *   - segments are ascending and disjoint, R / R+X / R+W only (W^X);
 *   - executable segments are transpiled word-by-word, so .text must hold
 *     nothing but valid rv64im instructions — no literal pools, no data;
 *   - the entrypoint is 4-aligned and instruction counting starts at the
 *     first executable segment's vaddr.
 * Guest memory is zero-initialized by the executor, so .bss needs no
 * clearing and NOBITS sections cost nothing in the ELF.
 */
ENTRY(_start)

PHDRS {
  text   PT_LOAD FLAGS(5); /* R+X */
  rodata PT_LOAD FLAGS(4); /* R   */
  data   PT_LOAD FLAGS(6); /* R+W */
}

SECTIONS {
  . = 0x78000000;

  .text : {
    KEEP(*(.text._start))
    *(.text .text.*)
  } :text

  . = ALIGN(4096);
  .rodata : {
    *(.rodata .rodata.*)
    *(.srodata .srodata.*)
  } :rodata

  . = ALIGN(4096);
  .data : {
    *(.data .data.*)
    *(.sdata .sdata.*)
  } :data
  PROVIDE(__global_pointer$ = .);

  .bss (NOLOAD) : {
    *(.sbss .sbss.*)
    *(.bss .bss.*)
    *(COMMON)
  } :data

  . = ALIGN(8);
  PROVIDE(_end = .);

  /DISCARD/ : {
    *(.eh_frame .eh_frame_hdr .note.gnu.build-id .comment .riscv.attributes)
  }
}
