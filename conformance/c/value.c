/* The encoding value.h describes.
 *
 * Four layers, each of which is the C of something with a name in the
 * Rust: the scalar parsers, which are crates/zu-common/src/temporal.rs
 * and the integer and float rules of value.rs; the type table, which is
 * the twenty names and their quoting; decode and payload, which walk a
 * node and refuse what does not fit; and show and same, which are what
 * a failure report and a comparison are made of.
 *
 * Nothing here reaches for a locale or a system clock. A date is
 * arithmetic on a day count, a time is arithmetic on a nanosecond
 * count, and a value that means one thing on a machine in Berlin and
 * another on a machine in Denver is not the sort of value a conformance
 * corpus can be written in.
 */
#include "value.h"

#include <inttypes.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- the arena ---- */

/* A row is a handful of values and a case is a handful of rows, so the
 * chunk is smaller than the reader's: a case that resets between rows
 * should not be holding a page it filled once. */
#define CV_CHUNK 4096
#define CV_ALIGN 8

typedef struct cv_chunk {
    struct cv_chunk *next;
    size_t len;
    size_t cap;
    char data[];
} cv_chunk;

struct cv_arena {
    cv_chunk *chunks;
};

cv_arena *cv_arena_new(void) {
    cv_arena *a = (cv_arena *)malloc(sizeof *a);
    if (a != NULL) {
        a->chunks = NULL;
    }
    return a;
}

void cv_arena_reset(cv_arena *a) {
    cv_chunk *c;
    if (a == NULL) {
        return;
    }
    for (c = a->chunks; c != NULL; c = c->next) {
        c->len = 0;
    }
}

void cv_arena_free(cv_arena *a) {
    cv_chunk *c;
    if (a == NULL) {
        return;
    }
    c = a->chunks;
    while (c != NULL) {
        cv_chunk *next = c->next;
        free(c);
        c = next;
    }
    free(a);
}

/* Also the runner's, which builds values of its own out of what the
 * engine handed back and wants them to live and die with the case the
 * decoded ones do. */
void *cv_alloc(cv_arena *a, size_t size) {
    size_t need = (size + (CV_ALIGN - 1)) & ~(size_t)(CV_ALIGN - 1);
    cv_chunk *c;
    void *p;
    /* A reset arena has its chunks in hand and empty, and the first one
     * is not always the biggest, so the walk stops at the first chunk
     * with room rather than at the head. */
    for (c = a->chunks; c != NULL; c = c->next) {
        if (c->cap - c->len >= need) {
            p = c->data + c->len;
            c->len += need;
            return p;
        }
    }
    {
        size_t cap = need > CV_CHUNK ? need : CV_CHUNK;
        c = (cv_chunk *)malloc(sizeof *c + cap);
        if (c == NULL) {
            return NULL;
        }
        c->next = a->chunks;
        c->len = need;
        c->cap = cap;
        a->chunks = c;
        return c->data;
    }
}

/* A copy of a string in the arena, NUL terminated so it can be printed
 * and length carrying because a corpus string may hold a NUL of its
 * own. A NULL ptr is the allocation having failed. */
static zy_str arena_str(cv_arena *a, const char *ptr, size_t len) {
    zy_str out;
    char *p = (char *)cv_alloc(a, len + 1);
    out.ptr = p;
    out.len = len;
    if (p != NULL) {
        memcpy(p, ptr, len);
        p[len] = '\0';
    }
    return out;
}

/* ---- errors ---- */

/* Always -1, so a caller writes `return fail(...)`. A line of zero
 * leaves the prefix off, for the one caller that has no node to take a
 * line from. */
static int fail(char *err, size_t err_len, size_t line, const char *fmt, ...) {
    va_list ap;
    size_t at = 0;
    if (err == NULL || err_len == 0) {
        return -1;
    }
    if (line > 0) {
        int n = snprintf(err, err_len, "line %lu: ", (unsigned long)line);
        if (n < 0 || (size_t)n >= err_len) {
            return -1;
        }
        at = (size_t)n;
    }
    va_start(ap, fmt);
    vsnprintf(err + at, err_len - at, fmt, ap);
    va_end(ap);
    return -1;
}

static int oom(char *err, size_t err_len) {
    return fail(err, err_len, 0, "out of memory");
}

/* ---- writing text out ---- */

/* A buffer that keeps counting after it is full, which is what lets
 * cv_show return the length its caller needed and a nested list be
 * written by the same function as a flat one. */
typedef struct cv_out {
    char *buf;
    size_t len;
    size_t used;
} cv_out;

static void emit(cv_out *o, const char *fmt, ...) {
    va_list ap;
    char *at = o->used < o->len ? o->buf + o->used : NULL;
    size_t space = o->used < o->len ? o->len - o->used : 0;
    int n;
    va_start(ap, fmt);
    n = vsnprintf(at, space, fmt, ap);
    va_end(ap);
    if (n > 0) {
        o->used += (size_t)n;
    }
}

/* A string as a case would write it in double quotes, which is the
 * escaping Rust's `{:?}` does: the five characters that have a short
 * escape get it, anything else below a space gets a hex escape, and
 * every other byte goes out as it came in so that a string of Vietnamese
 * or emoji stays readable in a report. */
static void emit_quoted(cv_out *o, zy_str s) {
    size_t i;
    emit(o, "\"");
    for (i = 0; i < s.len; i++) {
        unsigned char c = (unsigned char)s.ptr[i];
        switch (c) {
        case '"': emit(o, "\\\""); break;
        case '\\': emit(o, "\\\\"); break;
        case '\n': emit(o, "\\n"); break;
        case '\r': emit(o, "\\r"); break;
        case '\t': emit(o, "\\t"); break;
        default:
            if (c < 0x20 || c == 0x7f) {
                emit(o, "\\u{%x}", (unsigned)c);
            } else {
                emit(o, "%c", (int)c);
            }
        }
    }
    emit(o, "\"");
}

/* The same, into a buffer of its own, for an error message that has to
 * name the text it refused. A value long enough to fill this is a value
 * whose first hundred bytes say which one it is. */
static void quoted(zy_str s, char *buf, size_t len) {
    cv_out o;
    o.buf = buf;
    o.len = len;
    o.used = 0;
    emit_quoted(&o, s);
}

/* ---- integers ---- */

/* An integer in the range a type holds, parsed the way Rust's
 * `str::parse` does: an optional sign, then digits, then the end of the
 * text. No whitespace, no underscores, no other base, because a case is
 * written by a person and read by nine languages and every one of those
 * would have to agree about what `0x10` means.
 *
 * A minimum of zero is what says the type is unsigned, and an unsigned
 * type refuses a sign outright rather than accepting `-0`. */
static int parse_int(zy_str text, int64_t min, int64_t max, int64_t *out) {
    size_t i = 0;
    int negative = 0;
    uint64_t limit, mag = 0;
    if (text.len == 0) {
        return -1;
    }
    if (text.ptr[0] == '+' || text.ptr[0] == '-') {
        negative = text.ptr[0] == '-';
        if (negative && min == 0) {
            return -1;
        }
        i = 1;
    }
    if (i == text.len) {
        return -1;
    }
    limit = negative ? (uint64_t)(-(min + 1)) + 1 : (uint64_t)max;
    for (; i < text.len; i++) {
        unsigned digit;
        if (text.ptr[i] < '0' || text.ptr[i] > '9') {
            return -1;
        }
        digit = (unsigned)(text.ptr[i] - '0');
        if (mag > (limit - digit) / 10) {
            return -1;
        }
        mag = mag * 10 + digit;
    }
    /* Written this way round because the largest negative has no
     * positive of the same size to negate. */
    *out = negative ? -(int64_t)(mag - 1) - 1 : (int64_t)mag;
    return 0;
}

/* The same for a run of digits inside a larger literal, where there is
 * no sign to read and no width to check. */
static int digits(const char *ptr, size_t len, int64_t *out) {
    zy_str s;
    s.ptr = ptr;
    s.len = len;
    return parse_int(s, 0, INT64_MAX, out);
}

/* ---- floats ---- */

/* Whether the text is a float literal this encoding accepts, which is
 * narrower than what strtod takes. strtod reads hexadecimal floats,
 * leading whitespace and its own spellings of infinity, and a case that
 * meant any of those meant something a client in another language will
 * not read the same way. */
static int float_shaped(zy_str t) {
    size_t i = 0;
    int mantissa = 0;
    if (i < t.len && (t.ptr[i] == '+' || t.ptr[i] == '-')) {
        i++;
    }
    while (i < t.len && t.ptr[i] >= '0' && t.ptr[i] <= '9') {
        i++;
        mantissa = 1;
    }
    if (i < t.len && t.ptr[i] == '.') {
        i++;
        while (i < t.len && t.ptr[i] >= '0' && t.ptr[i] <= '9') {
            i++;
            mantissa = 1;
        }
    }
    if (!mantissa) {
        return 0;
    }
    if (i < t.len && (t.ptr[i] == 'e' || t.ptr[i] == 'E')) {
        int exponent = 0;
        i++;
        if (i < t.len && (t.ptr[i] == '+' || t.ptr[i] == '-')) {
            i++;
        }
        while (i < t.len && t.ptr[i] >= '0' && t.ptr[i] <= '9') {
            i++;
            exponent = 1;
        }
        if (!exponent) {
            return 0;
        }
    }
    return i == t.len;
}

/* Whether any of the characters in set appears in the text. A plain
 * strchr over the text would stop at a NUL, and a corpus string is
 * allowed to hold one. */
static int has_any(zy_str t, const char *set) {
    size_t i;
    for (i = 0; i < t.len; i++) {
        if (t.ptr[i] != '\0' && strchr(set, t.ptr[i]) != NULL) {
            return 1;
        }
    }
    return 0;
}

/* A float, including the three spellings YAML has no opinion about.
 *
 * They are spelled the way Rust's standard library prints them, so that
 * what a case writes and what a failure report prints are the same text
 * and a reader can compare them by eye. */
static int parse_float(zy_str text, double *out) {
    char *end;
    double v;
    if (zy_eq(text, "NaN")) {
        *out = (double)NAN;
        return 0;
    }
    if (zy_eq(text, "inf")) {
        *out = (double)INFINITY;
        return 0;
    }
    if (zy_eq(text, "-inf")) {
        *out = -(double)INFINITY;
        return 0;
    }
    /* A float is exact here, so `1` is not a FLOAT64 and neither is
     * `1e400`. The first is an integer somebody meant to write as `1.0`
     * and the second is `inf` under another name. */
    if (!has_any(text, ".eE") || !float_shaped(text)) {
        return -1;
    }
    v = strtod(text.ptr, &end);
    if (end != text.ptr + text.len || !isfinite(v)) {
        return -1;
    }
    *out = v;
    return 0;
}

/* ---- the calendar ---- */

#define NANOS_PER_SEC INT64_C(1000000000)
#define NANOS_PER_MINUTE (60 * NANOS_PER_SEC)
#define NANOS_PER_HOUR (60 * NANOS_PER_MINUTE)
#define NANOS_PER_DAY (24 * NANOS_PER_HOUR)

static int64_t floor_div(int64_t a, int64_t b) {
    int64_t q = a / b;
    return (a % b != 0 && ((a < 0) != (b < 0))) ? q - 1 : q;
}

static int64_t floor_mod(int64_t a, int64_t b) { return a - floor_div(a, b) * b; }

static int leap(int64_t year) { return year % 4 == 0 && (year % 100 != 0 || year % 400 == 0); }

static int64_t days_in(int64_t year, int64_t month) {
    switch (month) {
    case 1: case 3: case 5: case 7: case 8: case 10: case 12: return 31;
    case 4: case 6: case 9: case 11: return 30;
    default: return leap(year) ? 29 : 28;
    }
}

/* Days from 1970-01-01 to a proleptic Gregorian date, and back.
 *
 * This is Howard Hinnant's shift to a calendar whose year starts in
 * March, which makes the leap day the last day of the year and the
 * month lengths a repeating pattern with no table. It is the same
 * arithmetic as zu-common's, which is where the engine's dates come
 * from, so a date read here and a date read there are one number. */
static int64_t days_from_civil(int64_t year, int64_t month, int64_t day) {
    int64_t y = month <= 2 ? year - 1 : year;
    int64_t era = floor_div(y, 400);
    int64_t yoe = y - era * 400;
    int64_t m = month;
    int64_t doy = (153 * (m > 2 ? m - 3 : m + 9) + 2) / 5 + day - 1;
    int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146097 + doe - 719468;
}

static void civil_from_days(int64_t days, int64_t *year, int64_t *month, int64_t *day) {
    int64_t z = days + 719468;
    int64_t era = floor_div(z, 146097);
    int64_t doe = z - era * 146097;
    int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t y = yoe + era * 400;
    int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    int64_t mp = (5 * doy + 2) / 153;
    *day = doy - (153 * mp + 2) / 5 + 1;
    *month = mp < 10 ? mp + 3 : mp - 9;
    *year = *month <= 2 ? y + 1 : y;
}

/* `yyyy-mm-dd` as a day count. */
static int parse_date(zy_str t, int64_t *out) {
    int64_t year, month, day;
    if (t.len != 10 || t.ptr[4] != '-' || t.ptr[7] != '-') {
        return -1;
    }
    if (digits(t.ptr, 4, &year) != 0 || digits(t.ptr + 5, 2, &month) != 0 ||
        digits(t.ptr + 8, 2, &day) != 0) {
        return -1;
    }
    if (year < 1 || year > 9999 || month < 1 || month > 12 || day < 1 ||
        day > days_in(year, month)) {
        return -1;
    }
    *out = days_from_civil(year, month, day);
    return 0;
}

/* `ss` or `ss.fff` as whole seconds and nanoseconds. */
static int parse_seconds(zy_str t, int64_t *second, int64_t *nanos) {
    const char *dot = (const char *)memchr(t.ptr, '.', t.len);
    size_t whole = dot == NULL ? t.len : (size_t)(dot - t.ptr);
    *nanos = 0;
    if (dot != NULL) {
        size_t len = t.len - whole - 1;
        int64_t frac;
        int64_t scale = 1;
        size_t i;
        if (len == 0 || len > 9 || digits(dot + 1, len, &frac) != 0) {
            return -1;
        }
        for (i = len; i < 9; i++) {
            scale *= 10;
        }
        *nanos = frac * scale;
    }
    return digits(t.ptr, whole, second);
}

/* A written time split into the part before the zone and the zone in
 * minutes, with `has` clear when no zone is written.
 *
 * The caller has already taken the date off, so a sign here can only be
 * an offset: a time of day has no other use for one. */
static int split_offset(zy_str t, zy_str *body, int *has, int *offset) {
    size_t sign;
    int negative;
    zy_str zone;
    const char *colon;
    int64_t hours, minutes;
    *has = 0;
    *offset = 0;
    *body = t;
    if (t.len > 0 && (t.ptr[t.len - 1] == 'Z' || t.ptr[t.len - 1] == 'z')) {
        body->len = t.len - 1;
        *has = 1;
        return 0;
    }
    for (sign = 0; sign < t.len; sign++) {
        if (t.ptr[sign] == '+' || t.ptr[sign] == '-') {
            break;
        }
    }
    if (sign == t.len) {
        return 0;
    }
    body->len = sign;
    negative = t.ptr[sign] == '-';
    zone.ptr = t.ptr + sign + 1;
    zone.len = t.len - sign - 1;
    colon = (const char *)memchr(zone.ptr, ':', zone.len);
    if (colon != NULL) {
        size_t h = (size_t)(colon - zone.ptr);
        if (digits(zone.ptr, h, &hours) != 0 ||
            digits(colon + 1, zone.len - h - 1, &minutes) != 0) {
            return -1;
        }
    } else if (zone.len == 4) {
        /* `+0700` and `+07` are both written in the wild and both mean
         * the same offset. */
        if (digits(zone.ptr, 2, &hours) != 0 || digits(zone.ptr + 2, 2, &minutes) != 0) {
            return -1;
        }
    } else {
        if (digits(zone.ptr, zone.len, &hours) != 0) {
            return -1;
        }
        minutes = 0;
    }
    if (hours > 18 || minutes > 59) {
        return -1;
    }
    *has = 1;
    *offset = (int)(negative ? -(hours * 60 + minutes) : hours * 60 + minutes);
    return 0;
}

/* `hh:mm[:ss[.fffffffff]][offset]` as nanoseconds since midnight and
 * the offset in minutes when one is written. */
static int parse_time(zy_str t, int64_t *out, int *has, int *offset) {
    zy_str body, part;
    const char *first, *second;
    int64_t hour, minute, sec = 0, nanos = 0;
    if (split_offset(t, &body, has, offset) != 0) {
        return -1;
    }
    first = (const char *)memchr(body.ptr, ':', body.len);
    if (first == NULL) {
        return -1;
    }
    if (digits(body.ptr, (size_t)(first - body.ptr), &hour) != 0) {
        return -1;
    }
    second = (const char *)memchr(first + 1, ':', body.len - (size_t)(first + 1 - body.ptr));
    if (second == NULL) {
        if (digits(first + 1, body.len - (size_t)(first + 1 - body.ptr), &minute) != 0) {
            return -1;
        }
    } else {
        if (digits(first + 1, (size_t)(second - first - 1), &minute) != 0) {
            return -1;
        }
        part.ptr = second + 1;
        part.len = body.len - (size_t)(second + 1 - body.ptr);
        if (memchr(part.ptr, ':', part.len) != NULL || parse_seconds(part, &sec, &nanos) != 0) {
            return -1;
        }
    }
    if (hour > 23 || minute > 59 || sec > 59) {
        return -1;
    }
    *out = hour * NANOS_PER_HOUR + minute * NANOS_PER_MINUTE + sec * NANOS_PER_SEC + nanos;
    return 0;
}

/* `yyyy-mm-ddThh:mm:ss` as nanoseconds since the epoch, plus the offset
 * when one is written.
 *
 * A date on its own is midnight, which is what the standard's datetime
 * literal says and what the cast matrix expects of
 * `CAST('2024-01-15' AS DATETIME)`. There is no offset to read out of a
 * date, so the value is local until a zoned type asks for one and gets
 * UTC.
 *
 * A nanosecond count holds about 292 years either side of 1970 and the
 * calendar runs to 9999, so the multiplication is checked and a date
 * outside that range is refused. The Rust reader overflows there
 * instead, which is a bug in it and not a rule of the encoding; no case
 * has ever written one, and a reader that traps on a file is worse than
 * a reader that refuses it. */
static int parse_datetime(zy_str t, int64_t *out, int *has, int *offset) {
    const int64_t max_days = INT64_MAX / NANOS_PER_DAY;
    zy_str date, time;
    const char *split;
    int64_t days, nanos = 0;
    *has = 0;
    *offset = 0;
    split = (const char *)memchr(t.ptr, 'T', t.len);
    if (split == NULL) {
        split = (const char *)memchr(t.ptr, 't', t.len);
    }
    if (split == NULL) {
        split = (const char *)memchr(t.ptr, ' ', t.len);
    }
    if (split == NULL) {
        date = t;
    } else {
        date.ptr = t.ptr;
        date.len = (size_t)(split - t.ptr);
        time.ptr = split + 1;
        time.len = t.len - date.len - 1;
    }
    if (parse_date(date, &days) != 0 || days > max_days || days < -max_days) {
        return -1;
    }
    if (split != NULL && parse_time(time, &nanos, has, offset) != 0) {
        return -1;
    }
    if (days * NANOS_PER_DAY > INT64_MAX - nanos) {
        return -1;
    }
    *out = days * NANOS_PER_DAY + nanos;
    return 0;
}

/* A field times the size of its unit, with a fraction allowed because
 * `PT0.5S` is half a second and half a second is nanoseconds. */
static int scaled(zy_str t, int64_t unit, int64_t *out) {
    size_t i, whole_len = t.len;
    int64_t whole = 0, part = 0;
    for (i = 0; i < t.len; i++) {
        if (t.ptr[i] == '.' || t.ptr[i] == ',') {
            whole_len = i;
            break;
        }
    }
    if (whole_len > 0 && digits(t.ptr, whole_len, &whole) != 0) {
        return -1;
    }
    if (whole_len < t.len) {
        size_t len = t.len - whole_len - 1;
        int64_t frac, scale = 1;
        if (len == 0 || len > 9 || digits(t.ptr + whole_len + 1, len, &frac) != 0) {
            return -1;
        }
        for (i = 0; i < len; i++) {
            scale *= 10;
        }
        if (frac != 0 && unit > INT64_MAX / frac) {
            return -1;
        }
        part = unit * frac / scale;
    }
    if (whole != 0 && whole > INT64_MAX / unit) {
        return -1;
    }
    whole *= unit;
    if (whole > INT64_MAX - part) {
        return -1;
    }
    *out = whole + part;
    return 0;
}

static int add(int64_t *acc, int64_t n) {
    if (*acc > INT64_MAX - n) {
        return -1;
    }
    *acc += n;
    return 0;
}

/* One part of a duration, its number and unit pairs added into the
 * month count or the nanosecond count as the unit says.
 *
 * A unit with no number in front of it and a number with no unit after
 * it are both refused, which is what makes `P` and `PT1` errors rather
 * than a zero and a one of something unstated. */
static int fields(zy_str t, int time_part, int64_t *months, int64_t *nanos) {
    size_t i, start = 0;
    for (i = 0; i < t.len; i++) {
        char c = t.ptr[i];
        zy_str value;
        int64_t n, size;
        if ((c >= '0' && c <= '9') || c == '.' || c == ',') {
            continue;
        }
        if (i == start) {
            return -1;
        }
        value.ptr = t.ptr + start;
        value.len = i - start;
        start = i + 1;
        if (!time_part && (c == 'Y' || c == 'y' || c == 'M' || c == 'm')) {
            /* A third of a month is not a number of anything, so the
             * month fields take no fraction. */
            if (digits(value.ptr, value.len, &n) != 0) {
                return -1;
            }
            if (c == 'Y' || c == 'y') {
                if (n > INT64_MAX / 12) {
                    return -1;
                }
                n *= 12;
            }
            if (add(months, n) != 0) {
                return -1;
            }
            continue;
        }
        if (time_part) {
            switch (c) {
            case 'H': case 'h': size = NANOS_PER_HOUR; break;
            case 'M': case 'm': size = NANOS_PER_MINUTE; break;
            case 'S': case 's': size = NANOS_PER_SEC; break;
            default: return -1;
            }
        } else {
            switch (c) {
            case 'W': case 'w': size = 7 * NANOS_PER_DAY; break;
            case 'D': case 'd': size = NANOS_PER_DAY; break;
            default: return -1;
            }
        }
        if (scaled(value, size, &n) != 0 || add(nanos, n) != 0) {
            return -1;
        }
    }
    return start == t.len ? 0 : -1;
}

/* An ISO 8601 duration, `PnYnMnDTnHnMnS`.
 *
 * The two kinds are decided by which fields are written, and a duration
 * that writes both is refused rather than split, because a value that is
 * half months and half nanoseconds is exactly the thing the two kinds
 * exist to prevent. */
static int parse_duration(zy_str t, cv_unit *unit, int64_t *count) {
    size_t i;
    zy_str date_part, time_part;
    int negative = 0;
    int64_t months = 0, nanos = 0;
    if (t.len > 0 && (t.ptr[0] == '-' || t.ptr[0] == '+')) {
        negative = t.ptr[0] == '-';
        t.ptr++;
        t.len--;
    }
    if (t.len == 0 || (t.ptr[0] != 'P' && t.ptr[0] != 'p')) {
        return -1;
    }
    t.ptr++;
    t.len--;
    date_part = t;
    time_part.ptr = t.ptr + t.len;
    time_part.len = 0;
    for (i = 0; i < t.len; i++) {
        if (t.ptr[i] == 'T' || t.ptr[i] == 't') {
            date_part.len = i;
            time_part.ptr = t.ptr + i + 1;
            time_part.len = t.len - i - 1;
            break;
        }
    }
    if (date_part.len == 0 && time_part.len == 0) {
        return -1;
    }
    if (fields(date_part, 0, &months, &nanos) != 0 ||
        fields(time_part, 1, &months, &nanos) != 0) {
        return -1;
    }
    if (months != 0 && nanos != 0) {
        return -1;
    }
    if (nanos != 0 || months == 0) {
        *unit = CV_DAYTIME;
        *count = negative ? -nanos : nanos;
    } else {
        *unit = CV_YEARMONTH;
        *count = negative ? -months : months;
    }
    return 0;
}

/* A temporal of the type named, with the text already trimmed by the
 * reader that produced it. */
static int parse_temporal(cv_unit want, zy_str t, cv *out) {
    int64_t n;
    int has = 0, offset = 0;
    out->kind = CV_TEMPORAL;
    out->as.temporal.unit = want;
    out->as.temporal.offset = 0;
    switch (want) {
    case CV_DATE:
        if (parse_date(t, &n) != 0) {
            return -1;
        }
        out->as.temporal.count = n;
        return 0;
    case CV_LOCALTIME:
        /* A local type refuses a written zone rather than dropping it. */
        if (parse_time(t, &n, &has, &offset) != 0 || has) {
            return -1;
        }
        out->as.temporal.count = n;
        return 0;
    case CV_ZONEDTIME:
        if (parse_time(t, &n, &has, &offset) != 0) {
            return -1;
        }
        out->as.temporal.count = n;
        out->as.temporal.offset = offset;
        return 0;
    case CV_LOCALDATETIME:
        if (parse_datetime(t, &n, &has, &offset) != 0 || has) {
            return -1;
        }
        out->as.temporal.count = n;
        return 0;
    case CV_ZONEDDATETIME: {
        int64_t shift;
        if (parse_datetime(t, &n, &has, &offset) != 0) {
            return -1;
        }
        /* The stored instant is UTC, so a written offset is subtracted
         * rather than kept alongside a local time. Two values written in
         * two zones for one instant are then one number and compare
         * equal, which is the whole reason to store the instant. */
        shift = (int64_t)offset * NANOS_PER_MINUTE;
        if ((shift > 0 && n < INT64_MIN + shift) || (shift < 0 && n > INT64_MAX + shift)) {
            return -1;
        }
        out->as.temporal.count = n - shift;
        out->as.temporal.offset = offset;
        return 0;
    }
    default:
        return parse_duration(t, &out->as.temporal.unit, &out->as.temporal.count);
    }
}

/* ---- the types ---- */

/* Whether a type's payload is written as a quoted string, and why the
 * answer is not "whatever the writer felt like". Exact is a type a YAML
 * scalar carries without loss, Text is one it does not. */
typedef enum cv_form { CV_EXACT, CV_TEXT } cv_form;

typedef struct cv_type {
    const char *name;
    cv_form form;
} cv_type;

/* This is the GQL type of the value, not the C one. INT8 and INT64 are
 * both an int64_t here, and they are still two entries, because the
 * corpus is a contract for languages where they are not: a TypeScript
 * client returns `number` for one and `bigint` for the other, and a case
 * that did not say which meant nothing to it. */
static const cv_type TYPES[20] = {
    {"NULL", CV_EXACT},          {"BOOL", CV_EXACT},          {"INT8", CV_EXACT},
    {"INT16", CV_EXACT},         {"INT32", CV_EXACT},         {"INT64", CV_TEXT},
    {"UINT8", CV_EXACT},         {"UINT16", CV_EXACT},        {"UINT32", CV_EXACT},
    {"UINT64", CV_TEXT},         {"FLOAT32", CV_TEXT},        {"FLOAT64", CV_TEXT},
    {"STRING", CV_EXACT},        {"DATE", CV_TEXT},           {"LOCALTIME", CV_TEXT},
    {"ZONEDTIME", CV_TEXT},      {"LOCALDATETIME", CV_TEXT},  {"ZONEDDATETIME", CV_TEXT},
    {"DURATION", CV_TEXT},       {"LIST", CV_EXACT}};

/* The types the encoding reserves a name for and the engine has no
 * runtime value for yet, kept apart from an outright typo so that the
 * error says which of the two it is. */
static const char *const RESERVED[5] = {"DECIMAL", "BYTES", "NODE", "EDGE", "PATH"};

static const cv_type *type_of(const char *ty) {
    size_t i;
    for (i = 0; i < sizeof TYPES / sizeof TYPES[0]; i++) {
        if (strcmp(TYPES[i].name, ty) == 0) {
            return &TYPES[i];
        }
    }
    return NULL;
}

int cv_is_type(const char *ty) { return ty != NULL && type_of(ty) != NULL; }

/* What to say about a name that is not a type, which is one of two
 * things and worth telling apart. */
static int unknown(char *err, size_t err_len, size_t line, const char *ty) {
    size_t i;
    for (i = 0; i < sizeof RESERVED / sizeof RESERVED[0]; i++) {
        if (strcmp(RESERVED[i], ty) == 0) {
            return fail(err, err_len, line,
                        "%s is a type the encoding reserves and the engine has no value for", ty);
        }
    }
    return fail(err, err_len, line, "%s is not a type this encoding knows", ty);
}

/* The value a scalar payload spells, or -1 if it does not spell one of
 * that type. */
static int scalar(cv_arena *a, const char *ty, zy_str text, cv *out) {
    int64_t n;
    if (strcmp(ty, "BOOL") == 0) {
        out->kind = CV_BOOL;
        if (zy_eq(text, "true")) {
            out->as.boolean = 1;
            return 0;
        }
        if (zy_eq(text, "false")) {
            out->as.boolean = 0;
            return 0;
        }
        return -1;
    }
    if (strcmp(ty, "STRING") == 0) {
        out->kind = CV_STR;
        out->as.str = arena_str(a, text.ptr, text.len);
        /* Out of memory, which is not the text failing to be a STRING
         * and does not get that message. */
        return out->as.str.ptr == NULL ? -2 : 0;
    }
    if (strcmp(ty, "FLOAT32") == 0 || strcmp(ty, "FLOAT64") == 0) {
        double f;
        if (parse_float(text, &f) != 0) {
            return -1;
        }
        out->kind = CV_FLOAT;
        /* A FLOAT32 is narrowed so a case asserts what the narrower type
         * can hold, which is not what the wider one holds for the same
         * digits. */
        out->as.real = strcmp(ty, "FLOAT32") == 0 ? (double)(float)f : f;
        return 0;
    }
    if (strcmp(ty, "DATE") == 0) {
        return parse_temporal(CV_DATE, text, out);
    }
    if (strcmp(ty, "LOCALTIME") == 0) {
        return parse_temporal(CV_LOCALTIME, text, out);
    }
    if (strcmp(ty, "ZONEDTIME") == 0) {
        return parse_temporal(CV_ZONEDTIME, text, out);
    }
    if (strcmp(ty, "LOCALDATETIME") == 0) {
        return parse_temporal(CV_LOCALDATETIME, text, out);
    }
    if (strcmp(ty, "ZONEDDATETIME") == 0) {
        return parse_temporal(CV_ZONEDDATETIME, text, out);
    }
    if (strcmp(ty, "DURATION") == 0) {
        return parse_temporal(CV_DAYTIME, text, out);
    }
    out->kind = CV_INT;
    if (strcmp(ty, "INT8") == 0) {
        if (parse_int(text, INT8_MIN, INT8_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "INT16") == 0) {
        if (parse_int(text, INT16_MIN, INT16_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "INT32") == 0) {
        if (parse_int(text, INT32_MIN, INT32_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "INT64") == 0) {
        if (parse_int(text, INT64_MIN, INT64_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "UINT8") == 0) {
        if (parse_int(text, 0, UINT8_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "UINT16") == 0) {
        if (parse_int(text, 0, UINT16_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "UINT32") == 0) {
        if (parse_int(text, 0, UINT32_MAX, &n) != 0) {
            return -1;
        }
    } else if (strcmp(ty, "UINT64") == 0) {
        /* The engine's integer is signed and 64 bits wide, so the top
         * half of UINT64 has nowhere to go. Refusing it here is better
         * than wrapping it into a negative, which is a case that would
         * pass while meaning the opposite of what it says. */
        if (parse_int(text, 0, INT64_MAX, &n) != 0) {
            return -1;
        }
    } else {
        return -1;
    }
    out->as.integer = n;
    return 0;
}

int cv_payload(cv_arena *a, const char *ty, const zy_node *value, cv *out, char *err,
               size_t err_len) {
    const cv_type *type = type_of(ty);
    size_t line = value == NULL ? 0 : value->line;
    zy_str text;
    char shown[192];
    if (type == NULL) {
        return unknown(err, err_len, line, ty);
    }
    if (strcmp(ty, "LIST") == 0) {
        const zy_node *items;
        size_t count, i;
        cv *cells;
        /* The empty list is a value worth a case and needs a spelling,
         * which is a `value:` with nothing under it. */
        if (zy_seq_or_empty(value, &items, &count) != 0) {
            return fail(err, err_len, line, "a LIST holds a sequence of values, and this is %s",
                        zy_kind_name(value));
        }
        cells = count == 0 ? NULL : (cv *)cv_alloc(a, count * sizeof *cells);
        if (count > 0 && cells == NULL) {
            return oom(err, err_len);
        }
        for (i = 0; i < count; i++) {
            if (cv_decode(a, &items[i], &cells[i], err, err_len) != 0) {
                return -1;
            }
        }
        out->kind = CV_LIST;
        out->as.list.items = cells;
        out->as.list.count = count;
        return 0;
    }
    if (value == NULL || value->kind != ZY_SCALAR) {
        return fail(err, err_len, line, "a %s holds one scalar, and this is %s", ty,
                    zy_kind_name(value));
    }
    text = value->text;
    /* The one rule the whole encoding exists for, checked before the
     * text is looked at, because a value that parses is exactly the case
     * where a silent misread would survive review. */
    if (type->form == CV_TEXT && !value->quoted) {
        return fail(err, err_len, line,
                    "%s is written in quotes, because a bare %s is a number and some reader of "
                    "this file will round it",
                    ty, text.ptr);
    }
    if (type->form == CV_EXACT && value->quoted && strcmp(ty, "STRING") != 0) {
        return fail(err, err_len, line,
                    "%s is written without quotes, so that a reader cannot take it for a string",
                    ty);
    }
    switch (scalar(a, ty, text, out)) {
    case 0:
        return 0;
    case -2:
        return oom(err, err_len);
    default:
        quoted(text, shown, sizeof shown);
        return fail(err, err_len, line, "%s is not a %s", shown, ty);
    }
}

int cv_decode(cv_arena *a, const zy_node *node, cv *out, char *err, size_t err_len) {
    static const char *const KEYS[2] = {"type", "value"};
    size_t line = node == NULL ? 0 : node->line;
    zy_str extra;
    if (node == NULL || node->kind != ZY_MAP) {
        return fail(err, err_len, line,
                    "a value is a mapping of `type` and `value`, and this is %s",
                    zy_kind_name(node));
    }
    extra = zy_unknown(node, KEYS, 2);
    if (extra.ptr != NULL) {
        return fail(err, err_len, line, "a value has no key \"%s\"", extra.ptr);
    }
    return cv_typed(a, node, out, err, err_len);
}

int cv_typed(cv_arena *a, const zy_node *node, cv *out, char *err, size_t err_len) {
    size_t line = node == NULL ? 0 : node->line;
    const zy_node *type, *value;
    const char *ty;
    type = zy_get(node, "type");
    if (type == NULL) {
        return fail(err, err_len, line, "a value with no `type`");
    }
    if (type->kind != ZY_SCALAR) {
        return fail(err, err_len, line, "a `type` that is not a name");
    }
    ty = type->text.ptr;
    /* Checked here as well as in cv_payload, because a value whose type
     * is not a type and which also has no `value` under it should be
     * told about the type first: that is the mistake, and the missing
     * payload is a consequence of it. */
    if (type_of(ty) == NULL) {
        return unknown(err, err_len, line, ty);
    }
    value = zy_get(node, "value");
    if (strcmp(ty, "NULL") == 0) {
        if (value != NULL) {
            return fail(err, err_len, line, "NULL carries no `value`");
        }
        out->kind = CV_NULL;
        return 0;
    }
    if (value == NULL) {
        return fail(err, err_len, line, "a %s with no `value`", ty);
    }
    return cv_payload(a, ty, value, out, err, err_len);
}

/* ---- showing ---- */

/* The shortest text that reads back as the same double, written the way
 * the Rust reader writes it so that a value printed by either can be
 * pasted into a case.
 *
 * Two decisions, and both are Rust's `{:?}` rather than printf's. The
 * digits are the fewest that round trip, which `%g` does not give and
 * `%.17g` gives too many of. The shape is positional between a ten
 * thousandth and ten to the sixteenth and an exponent outside that,
 * which is a threshold printf states in significant digits and this
 * states in the size of the number. */
static void emit_float(cv_out *o, double f) {
    char buf[64];
    double size = f < 0 ? -f : f;
    const char *at;
    int p, exponent;
    if (isnan(f)) {
        emit(o, "NaN");
        return;
    }
    if (isinf(f)) {
        emit(o, f < 0 ? "-inf" : "inf");
        return;
    }
    for (p = 1; p < 17; p++) {
        snprintf(buf, sizeof buf, "%.*e", p - 1, f);
        if (strtod(buf, NULL) == f) {
            break;
        }
    }
    if (p == 17) {
        snprintf(buf, sizeof buf, "%.16e", f);
    }
    at = strchr(buf, 'e');
    exponent = at == NULL ? 0 : atoi(at + 1);
    if (f == 0.0 || (size >= 1e-4 && size < 1e16)) {
        /* A whole number keeps a fractional digit, because `1` is an
         * integer in this encoding and `1.0` is the float. */
        int fraction = p - 1 - exponent;
        emit(o, "%.*f", fraction < 1 ? 1 : fraction, f);
        return;
    }
    /* `1e+16` and `1e16` are the same double and the second is what a
     * case would be written with, so the exponent loses its plus and its
     * leading zeros on the way out. */
    if (at != NULL) {
        emit(o, "%.*se%d", (int)(at - buf), buf, exponent);
    }
}

static void emit_time(cv_out *o, int64_t nanos) {
    int64_t n = floor_mod(nanos, NANOS_PER_DAY);
    int64_t frac = n % NANOS_PER_SEC;
    emit(o, "%02" PRId64 ":%02" PRId64 ":%02" PRId64, n / NANOS_PER_HOUR,
         n / NANOS_PER_MINUTE % 60, n / NANOS_PER_SEC % 60);
    if (frac != 0) {
        emit(o, ".%09" PRId64, frac);
    }
}

static void emit_date(cv_out *o, int64_t days) {
    int64_t y, m, d;
    civil_from_days(days, &y, &m, &d);
    emit(o, "%04" PRId64 "-%02" PRId64 "-%02" PRId64, y, m, d);
}

static void emit_datetime(cv_out *o, int64_t nanos) {
    emit_date(o, floor_div(nanos, NANOS_PER_DAY));
    emit(o, "T");
    emit_time(o, floor_mod(nanos, NANOS_PER_DAY));
}

static void emit_offset(cv_out *o, int offset) {
    int magnitude = offset < 0 ? -offset : offset;
    if (offset == 0) {
        emit(o, "Z");
        return;
    }
    emit(o, "%c%02d:%02d", offset < 0 ? '-' : '+', magnitude / 60, magnitude % 60);
}

/* A duration written the way ISO writes it, which is the way it parses
 * back. A zero duration is `PT0S`, because `P` alone is not a value. */
static void emit_duration(cv_out *o, cv_unit unit, int64_t count) {
    uint64_t magnitude;
    if (count < 0) {
        emit(o, "-");
    }
    magnitude = count < 0 ? (uint64_t)(-(count + 1)) + 1 : (uint64_t)count;
    emit(o, "P");
    if (unit == CV_YEARMONTH) {
        uint64_t years = magnitude / 12, months = magnitude % 12;
        if (years != 0) {
            emit(o, "%" PRIu64 "Y", years);
        }
        if (months != 0 || years == 0) {
            emit(o, "%" PRIu64 "M", months);
        }
        return;
    }
    {
        uint64_t days = magnitude / (uint64_t)NANOS_PER_DAY;
        int64_t rest = (int64_t)(magnitude % (uint64_t)NANOS_PER_DAY);
        int64_t h = rest / NANOS_PER_HOUR, m = rest / NANOS_PER_MINUTE % 60;
        int64_t s = rest / NANOS_PER_SEC % 60, frac = rest % NANOS_PER_SEC;
        if (days != 0) {
            emit(o, "%" PRIu64 "D", days);
        }
        if (rest == 0 && days != 0) {
            return;
        }
        emit(o, "T");
        if (h != 0) {
            emit(o, "%" PRId64 "H", h);
        }
        if (m != 0) {
            emit(o, "%" PRId64 "M", m);
        }
        if (s != 0 || frac != 0 || (h == 0 && m == 0)) {
            emit(o, "%" PRId64, s);
            if (frac != 0) {
                emit(o, ".%09" PRId64, frac);
            }
            emit(o, "S");
        }
    }
}

/* The encoding's name for a unit, for a report that has to name the
 * type it found. Both duration kinds are one type name, as they are one
 * type: which of the two a value is shows in how it prints. */
static const char *unit_name(cv_unit unit) {
    switch (unit) {
    case CV_DATE: return "DATE";
    case CV_LOCALTIME: return "LOCALTIME";
    case CV_ZONEDTIME: return "ZONEDTIME";
    case CV_LOCALDATETIME: return "LOCALDATETIME";
    case CV_ZONEDDATETIME: return "ZONEDDATETIME";
    default: return "DURATION";
    }
}

static void emit_value(cv_out *o, const cv *v) {
    size_t i;
    switch (v->kind) {
    case CV_NULL:
        emit(o, "NULL");
        return;
    case CV_BOOL:
        emit(o, "BOOL %s", v->as.boolean ? "true" : "false");
        return;
    case CV_INT:
        emit(o, "INT64 \"%" PRId64 "\"", v->as.integer);
        return;
    case CV_FLOAT:
        emit(o, "FLOAT64 \"");
        emit_float(o, v->as.real);
        emit(o, "\"");
        return;
    case CV_STR:
        emit(o, "STRING ");
        emit_quoted(o, v->as.str);
        return;
    case CV_TEMPORAL:
        emit(o, "%s \"", unit_name(v->as.temporal.unit));
        switch (v->as.temporal.unit) {
        case CV_DATE: emit_date(o, v->as.temporal.count); break;
        case CV_LOCALTIME: emit_time(o, v->as.temporal.count); break;
        case CV_ZONEDTIME:
            emit_time(o, v->as.temporal.count);
            emit_offset(o, v->as.temporal.offset);
            break;
        case CV_LOCALDATETIME: emit_datetime(o, v->as.temporal.count); break;
        case CV_ZONEDDATETIME:
            emit_datetime(o, v->as.temporal.count +
                                 (int64_t)v->as.temporal.offset * NANOS_PER_MINUTE);
            emit_offset(o, v->as.temporal.offset);
            break;
        default: emit_duration(o, v->as.temporal.unit, v->as.temporal.count); break;
        }
        emit(o, "\"");
        return;
    default:
        emit(o, "LIST [");
        for (i = 0; i < v->as.list.count; i++) {
            if (i > 0) {
                emit(o, ", ");
            }
            emit_value(o, &v->as.list.items[i]);
        }
        emit(o, "]");
        return;
    }
}

size_t cv_show(const cv *v, char *buf, size_t len) {
    cv_out o;
    o.buf = buf;
    o.len = len;
    o.used = 0;
    if (len > 0) {
        buf[0] = '\0';
    }
    if (v == NULL) {
        return 0;
    }
    emit_value(&o, v);
    return o.used;
}

/* ---- comparing ---- */

static uint64_t bits(double f) {
    uint64_t out;
    memcpy(&out, &f, sizeof out);
    return out;
}

int cv_same(const cv *a, const cv *b) {
    size_t i;
    if (a == NULL || b == NULL) {
        return a == b;
    }
    if (a->kind != b->kind) {
        return 0;
    }
    switch (a->kind) {
    case CV_NULL:
        return 1;
    case CV_BOOL:
        return (a->as.boolean != 0) == (b->as.boolean != 0);
    case CV_INT:
        return a->as.integer == b->as.integer;
    case CV_FLOAT:
        if (isnan(a->as.real) && isnan(b->as.real)) {
            return 1;
        }
        return bits(a->as.real) == bits(b->as.real);
    case CV_STR:
        return a->as.str.len == b->as.str.len &&
               memcmp(a->as.str.ptr, b->as.str.ptr, a->as.str.len) == 0;
    case CV_TEMPORAL:
        return a->as.temporal.unit == b->as.temporal.unit &&
               a->as.temporal.count == b->as.temporal.count &&
               a->as.temporal.offset == b->as.temporal.offset;
    default:
        if (a->as.list.count != b->as.list.count) {
            return 0;
        }
        for (i = 0; i < a->as.list.count; i++) {
            if (!cv_same(&a->as.list.items[i], &b->as.list.items[i])) {
                return 0;
            }
        }
        return 1;
    }
}
