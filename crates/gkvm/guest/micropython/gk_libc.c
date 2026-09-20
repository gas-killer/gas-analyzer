/* The str* half of a libc, for a guest that links none.
 *
 * gk-guest-crt already provides memset/memcpy/memmove/memcmp (MicroPython's
 * shared/libc/string0.c would define them a second time), and the distro's
 * libc multilibs are not guaranteed to be pure rv64im — so the handful of
 * string functions the interpreter calls live here. Headers still come from
 * the toolchain's libc; only declarations are used.
 */
#include <stddef.h>
#include <string.h>

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = s;
    for (; n > 0; n--, p++) {
        if (*p == (unsigned char)c) {
            return (void *)p;
        }
    }
    return NULL;
}

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) {
        p++;
    }
    return (size_t)(p - s);
}

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++, b++;
    }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    for (; n > 0; n--, a++, b++) {
        if (*a != *b || !*a) {
            return (unsigned char)*a - (unsigned char)*b;
        }
    }
    return 0;
}

char *strcpy(char *dst, const char *src) {
    char *d = dst;
    while ((*d++ = *src++)) {
    }
    return dst;
}

char *strncpy(char *dst, const char *src, size_t n) {
    size_t i = 0;
    for (; i < n && src[i]; i++) {
        dst[i] = src[i];
    }
    for (; i < n; i++) {
        dst[i] = 0;
    }
    return dst;
}

char *strcat(char *dst, const char *src) {
    strcpy(dst + strlen(dst), src);
    return dst;
}

char *strchr(const char *s, int c) {
    for (;; s++) {
        if (*s == (char)c) {
            return (char *)s;
        }
        if (!*s) {
            return NULL;
        }
    }
}

char *strstr(const char *haystack, const char *needle) {
    size_t n = strlen(needle);
    for (; *haystack; haystack++) {
        if (strncmp(haystack, needle, n) == 0) {
            return (char *)haystack;
        }
    }
    return n == 0 ? (char *)haystack : NULL;
}
