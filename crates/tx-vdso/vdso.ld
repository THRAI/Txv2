/* RISC-V vDSO linker script.

   Produces a minimal shared object with:
     - ELF header at vaddr 0
     - .text      (RX)
     - .dynamic   (R)
     - .dynsym    (R)
     - .dynstr    (R)
     - .hash      (R)

   No .data, no .bss — the stub symbols are pure code.
   No PHDRS — the linker auto-generates PT_LOAD and PT_DYNAMIC.
*/

ENTRY(__vdso_clock_gettime)

SECTIONS
{
    /* ELF + program headers: mapped at vdso_base+0. */
    . = SIZEOF_HEADERS;

    .text : {
        *(.text)
    }

    .dynamic : {
        *(.dynamic)
    }

    .dynsym : {
        *(.dynsym)
    }

    .dynstr : {
        *(.dynstr)
    }

    .hash : {
        *(.hash)
    }

    .gnu.hash : {
        *(.gnu.hash)
    }

    /* Discard everything else to keep the image minimal. */
    /DISCARD/ : {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
        *(.got*)
        *(.plt*)
        *(.rela*)
        *(.gnu.version*)
        *(.gnu.attributes)
        *(.riscv.attributes)
        *(.debug*)
        *(.stab*)
        *(.shstrtab)
    }
}
