/* blitz.h - C ABI for headless Blitz HTML rendering.
 *
 * Threading: BlitzContext is safe to share across threads; renders are
 * serialised internally. Error messages are per-thread.
 *
 * Ownership: every non-NULL BlitzImage / BlitzBuffer written by this library
 * must be released with the matching blitz_*_free. Do not call free().
 */
#ifndef BLITZ_H
#define BLITZ_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BLITZ_OK                 0
#define BLITZ_ERR_INVALID_ARG   -1
#define BLITZ_ERR_INVALID_UTF8  -2
#define BLITZ_ERR_INVALID_URL   -3
#define BLITZ_ERR_NETWORK       -4
#define BLITZ_ERR_IO            -5
#define BLITZ_ERR_RENDER        -6
#define BLITZ_ERR_PANIC         -7

#define BLITZ_COLOR_SCHEME_LIGHT 0u
#define BLITZ_COLOR_SCHEME_DARK  1u

/* Which @media rules apply. print is for producing a document rather than a
 * screenshot; it is styling only and does NOT paginate — @page and
 * page-break-* are parsed and ignored. */
#define BLITZ_MEDIA_TYPE_SCREEN  0u
#define BLITZ_MEDIA_TYPE_PRINT   1u

typedef struct BlitzContext BlitzContext;

typedef struct {
    uint32_t    width;              /* CSS px; 0 -> 1200                     */
    uint32_t    height;             /* CSS px; 0 -> 800                      */
    float       scale;              /* device pixel ratio; <=0 -> 1.0        */
    uint32_t    max_height;         /* CSS px cap; 0 -> 4000                 */
    uint32_t    color_scheme;       /* BLITZ_COLOR_SCHEME_*                  */
    uint32_t    background_rgba;    /* 0xRRGGBBAA; alpha 0 -> transparent    */
    uint32_t    net_timeout_ms;     /* 0 -> 10000                            */
    const char *user_agent;         /* NULL -> library default               */
    uint8_t     enable_net;         /* 0 disables sub-resource fetching      */
    uint8_t     fit_content_height; /* grow height to fit document           */
    uint8_t     media_type;         /* BLITZ_MEDIA_TYPE_*                    */
    uint8_t     _reserved[1];
} BlitzRenderOptions;

typedef struct {
    uint8_t *data;   /* RGBA8, row-major */
    size_t   len;
    size_t   cap;    /* allocator bookkeeping; do not modify */
    uint32_t width;  /* device px */
    uint32_t height; /* device px */
    uint32_t stride; /* bytes per row */
} BlitzImage;

typedef struct {
    uint8_t *data;
    size_t   len;
    size_t   cap;    /* allocator bookkeeping; do not modify */
} BlitzBuffer;

const char *blitz_version(void);

/* Message for the last failure on this thread, or NULL. Invalidated by the
 * next blitz_* call on this thread. */
const char *blitz_last_error_message(void);

BlitzRenderOptions blitz_render_options_default(void);

/* worker_threads == 0 lets tokio choose. Returns NULL on failure. */
BlitzContext *blitz_context_new(uint32_t worker_threads);
void          blitz_context_free(BlitzContext *ctx);

/* opts may be NULL to accept all defaults. */
int blitz_render_html(BlitzContext       *ctx,
                      const char         *html,
                      const char         *base_url, /* nullable */
                      const BlitzRenderOptions *opts,
                      BlitzImage         *out);

int blitz_render_url(BlitzContext       *ctx,
                     const char         *url,
                     const BlitzRenderOptions *opts,
                     BlitzImage         *out);

/* Render markdown (GFM: tables, footnotes, strikethrough, task lists).
 *
 * stylesheet: NULL -> built-in sheet, "" -> unstyled, otherwise used verbatim.
 * base_url:   nullable; needed for relative image paths.
 */
int blitz_render_markdown(BlitzContext       *ctx,
                          const char         *markdown,
                          const char         *base_url,   /* nullable */
                          const char         *stylesheet, /* nullable */
                          const BlitzRenderOptions *opts,
                          BlitzImage         *out);

/* Markdown -> styled HTML document, no rendering. The buffer holds UTF-8 and is
 * NOT NUL-terminated; use out->len. Free with blitz_buffer_free. */
int blitz_markdown_to_html(const char  *markdown,
                           const char  *stylesheet, /* nullable */
                           BlitzBuffer *out);

/* Static, never freed. */
const char *blitz_default_markdown_stylesheet(void);

/* dpi == 0 -> 144 */
int blitz_image_encode_png(const BlitzImage *image, uint32_t dpi, BlitzBuffer *out);
int blitz_image_write_png(const BlitzImage *image, const char *path, uint32_t dpi);

void blitz_image_free(BlitzImage *image);
void blitz_buffer_free(BlitzBuffer *buf);

#ifdef __cplusplus
}
#endif

#endif /* BLITZ_H */
