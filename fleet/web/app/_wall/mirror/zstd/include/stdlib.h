/* Freestanding stand-in for zdec.c's wasm32 build: zdec.c defines these, and nothing calls them. */
#pragma once
#include <stddef.h>
void *malloc(size_t n);
void *calloc(size_t n, size_t m);
void free(void *p);
