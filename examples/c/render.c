/* render.c - render whatever you point it at.
 *
 *   ./render https://example.com
 *   ./render README.md
 *   ./render docs/index.html out.png 1400
 *
 * Dispatches on the input:
 *   looks like a URL  -> blitz_render_url      (fetched over HTTP, or file://)
 *   .md / .markdown   -> blitz_render_markdown (read locally)
 *   anything else     -> blitz_render_html     (read locally)
 *
 * Local files get a `file://` base URL for their containing directory, so
 * relative <img src> and stylesheet paths resolve.
 */
/* realpath() and strcasecmp() are XSI, not plain POSIX — _POSIX_C_SOURCE alone
 * leaves them undeclared under -std=c11. 700 implies POSIX 2008 as well. */
#define _XOPEN_SOURCE 700

#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>

#include "blitz.h"

#define DEFAULT_WIDTH 1200

static void report(const char *what, int rc)
{
    const char *msg = blitz_last_error_message();
    fprintf(stderr, "%s failed (rc=%d): %s\n", what, rc, msg ? msg : "no detail");
}

/* A scheme followed by "://" means URL; anything else is a path. Deliberately
 * crude — it just has to separate "https://x" and "file:///x" from "./x.md",
 * and a Windows "C:\..." has no "//" so it lands on the path side. */
static int looks_like_url(const char *s)
{
    const char *sep = strstr(s, "://");
    if (sep == NULL || sep == s) {
        return 0;
    }
    for (const char *p = s; p < sep; p++) {
        int ok = (*p >= 'a' && *p <= 'z') || (*p >= 'A' && *p <= 'Z')
              || (*p >= '0' && *p <= '9') || *p == '+' || *p == '-' || *p == '.';
        if (!ok) {
            return 0;
        }
    }
    return 1;
}

static int has_suffix_ci(const char *s, const char *suffix)
{
    size_t ls = strlen(s), lx = strlen(suffix);
    return ls >= lx && strcasecmp(s + ls - lx, suffix) == 0;
}

static int is_markdown(const char *path)
{
    return has_suffix_ci(path, ".md")
        || has_suffix_ci(path, ".markdown")
        || has_suffix_ci(path, ".mdown")
        || has_suffix_ci(path, ".mkd");
}

/* Read a whole file as a NUL-terminated string. The FFI takes C strings, so an
 * embedded NUL would truncate the document — worth knowing, though it can't
 * happen with valid UTF-8 text. Caller frees. */
static char *read_file(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (f == NULL) {
        perror(path);
        return NULL;
    }

    char  *buf = NULL;
    size_t len = 0, cap = 1 << 16;

    buf = malloc(cap);
    if (buf == NULL) {
        fclose(f);
        fprintf(stderr, "out of memory\n");
        return NULL;
    }

    /* Read incrementally rather than trusting fseek/ftell, which don't give a
     * usable size for pipes or /dev/stdin. */
    for (;;) {
        if (len + 4096 + 1 > cap) {
            cap *= 2;
            char *grown = realloc(buf, cap);
            if (grown == NULL) {
                free(buf);
                fclose(f);
                fprintf(stderr, "out of memory\n");
                return NULL;
            }
            buf = grown;
        }
        size_t n = fread(buf + len, 1, 4096, f);
        len += n;
        if (n < 4096) {
            break;
        }
    }

    int bad = ferror(f);
    fclose(f);
    if (bad) {
        fprintf(stderr, "%s: read error\n", path);
        free(buf);
        return NULL;
    }

    buf[len] = '\0';
    return buf;
}

/* "docs/index.html" -> "file:///abs/path/to/docs/", which is what relative URLs
 * inside the document resolve against. Caller frees.
 *
 * Note this does no percent-encoding, so a path containing spaces or '#' will
 * produce a base URL that parses oddly. Fine for an example; a real tool should
 * encode it. */
static char *base_url_for(const char *path)
{
    char resolved[PATH_MAX];
    if (realpath(path, resolved) == NULL) {
        perror(path);
        return NULL;
    }

    char *slash = strrchr(resolved, '/');
    if (slash != NULL) {
        slash[1] = '\0'; /* keep the trailing slash: it marks a directory */
    }

    size_t need = strlen("file://") + strlen(resolved) + 1;
    char  *url  = malloc(need);
    if (url == NULL) {
        fprintf(stderr, "out of memory\n");
        return NULL;
    }
    snprintf(url, need, "file://%s", resolved);
    return url;
}

/* Pick an output filename when the caller didn't give one.
 *
 *   docs/index.html        -> index.png
 *   https://www.google.com -> wwwgooglecom.png
 *
 * Splitting a URL on '.' would yield "www.google.png", so URLs get the
 * alphanumeric-squash treatment from the original screenshot example instead.
 * Caller frees. */
static char *derive_output(const char *input)
{
    char *out;

    if (looks_like_url(input)) {
        const char *host = strstr(input, "://");
        host = host ? host + 3 : input;

        out = malloc(12 + 5);
        if (out == NULL) {
            fprintf(stderr, "out of memory\n");
            return NULL;
        }

        size_t n = 0;
        for (const char *p = host; *p != '\0' && n < 12; p++) {
            unsigned char c = (unsigned char)*p;
            if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
                || (c >= '0' && c <= '9')) {
                out[n++] = (char)c;
            }
        }
        if (n == 0) {
            memcpy(out, "output", 6);
            n = 6;
        }
        memcpy(out + n, ".png", 5);
        return out;
    }

    const char *slash = strrchr(input, '/');
    const char *base  = slash ? slash + 1 : input;
    if (*base == '\0') {
        base = "output";
    }

    const char *dot  = strrchr(base, '.');
    size_t      stem = (dot && dot != base) ? (size_t)(dot - base) : strlen(base);

    out = malloc(stem + 5);
    if (out == NULL) {
        fprintf(stderr, "out of memory\n");
        return NULL;
    }
    memcpy(out, base, stem);
    memcpy(out + stem, ".png", 5);
    return out;
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s <url|file.html|file.md> [out.png] [width]\n",
                argv[0]);
        return 2;
    }

    const char *input = argv[1];
    int         wide  = (argc > 3) ? atoi(argv[3]) : DEFAULT_WIDTH;
    if (wide <= 0) {
        fprintf(stderr, "width must be positive, got %s\n", argv[3]);
        return 2;
    }

    char *derived_out = NULL;
    const char *out;
    if (argc > 2) {
        out = argv[2];
    } else {
        derived_out = derive_output(input);
        if (derived_out == NULL) {
            return 1;
        }
        out = derived_out;
    }

    BlitzContext *ctx = blitz_context_new(0);
    if (ctx == NULL) {
        report("blitz_context_new", -1);
        free(derived_out);
        return 1;
    }

    BlitzRenderOptions opts = blitz_render_options_default();
    opts.width  = (uint32_t)wide;
    opts.scale  = 2.0f;
    opts.height = 800;

    BlitzImage img    = {0};
    char      *source = NULL;
    char      *base   = NULL;
    int        status = 1;
    int        rc;

    if (looks_like_url(input)) {
        printf("url      %s\n", input);
        rc = blitz_render_url(ctx, input, &opts, &img);
        if (rc != BLITZ_OK) {
            report("blitz_render_url", rc);
            goto cleanup;
        }
    } else {
        int markdown = is_markdown(input);
        printf("%s %s\n", markdown ? "markdown" : "html    ", input);

        source = read_file(input);
        if (source == NULL) {
            goto cleanup;
        }

        /* Non-fatal: without a base URL, relative asset paths just don't load. */
        base = base_url_for(input);

        if (markdown) {
            /* NULL stylesheet -> the built-in sheet. Pass a CSS string here to
             * override it, or "" for an unstyled document. */
            rc = blitz_render_markdown(ctx, source, base, NULL, &opts, &img);
            if (rc != BLITZ_OK) {
                report("blitz_render_markdown", rc);
                goto cleanup;
            }
        } else {
            rc = blitz_render_html(ctx, source, base, &opts, &img);
            if (rc != BLITZ_OK) {
                report("blitz_render_html", rc);
                goto cleanup;
            }
        }
    }

    rc = blitz_image_write_png(&img, out, 144);
    if (rc != BLITZ_OK) {
        report("blitz_image_write_png", rc);
        goto cleanup;
    }

    printf("wrote    %s (%ux%u)\n", out, img.width, img.height);
    status = 0;

cleanup:
    blitz_image_free(&img);
    blitz_context_free(ctx);
    free(source);
    free(base);
    free(derived_out);
    return status;
}
