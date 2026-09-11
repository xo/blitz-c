/* screenshot.c - render a page to a PNG using libblitz.
 *
 *   ./screenshot                                  -> google.png
 *   ./screenshot https://example.com out.png 1400
 *
 * Demonstrates the full lifecycle: one context, one render, one encode, and a
 * free for everything the library handed back.
 */
/* clock_gettime is POSIX, not ISO C, and -std=c11 hides it without this. */
#define _POSIX_C_SOURCE 199309L

#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#include "blitz.h"

#define DEFAULT_URL   "https://www.google.com"
#define DEFAULT_OUT   "google.png"
#define DEFAULT_WIDTH 1200

/* Every failure path looks the same: the return code says what kind of problem
 * it was, blitz_last_error_message says which one specifically. The message is
 * only valid until the next blitz_* call on this thread, so print it now. */
static void report(const char *what, int rc)
{
    const char *msg = blitz_last_error_message();
    fprintf(stderr, "%s failed (rc=%d): %s\n", what, rc, msg ? msg : "no detail");
}

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1.0e6;
}

int main(int argc, char **argv)
{
    const char *url  = (argc > 1) ? argv[1] : DEFAULT_URL;
    const char *out  = (argc > 2) ? argv[2] : DEFAULT_OUT;
    int         wide = (argc > 3) ? atoi(argv[3]) : DEFAULT_WIDTH;

    if (wide <= 0) {
        fprintf(stderr, "width must be positive, got %s\n", argv[3]);
        return 2;
    }

    printf("libblitz %s\n", blitz_version());
    printf("rendering %s\n", url);

    /* Spins up the tokio runtime that backs sub-resource fetching. Expensive,
     * so a real program creates one of these at startup and keeps it. */
    BlitzContext *ctx = blitz_context_new(0);
    if (ctx == NULL) {
        report("blitz_context_new", -1);
        return 1;
    }

    int status = 1;

    BlitzRenderOptions opts = blitz_render_options_default();
    opts.width  = (uint32_t)wide;
    opts.scale  = 2.0f;  /* 2x, as the original example did */
    opts.height = 800;   /* minimum; grows to fit the document */

    BlitzImage img = {0};

    double t0 = now_ms();
    int rc = blitz_render_url(ctx, url, &opts, &img);
    double t1 = now_ms();

    if (rc != BLITZ_OK) {
        report("blitz_render_url", rc);
        goto cleanup_ctx;
    }

    printf("rendered %ux%u in %.0fms\n", img.width, img.height, t1 - t0);

    rc = blitz_image_write_png(&img, out, 144);
    if (rc != BLITZ_OK) {
        report("blitz_image_write_png", rc);
        goto cleanup_img;
    }

    printf("wrote %s in %.0fms\n", out, now_ms() - t1);
    status = 0;

    /* The image owns Rust-allocated memory. blitz_image_free, never free(). */
cleanup_img:
    blitz_image_free(&img);
cleanup_ctx:
    blitz_context_free(ctx);
    return status;
}
