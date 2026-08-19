/* The reader yaml.h describes. Three passes, none of them clever.
 *
 * The lexer drops blanks and comments, refuses what the subset does not
 * hold, and splits every `- ` into the item it opens and the content
 * that followed it on the same line. Splitting there rather than in the
 * parser is what lets `- name: x` and a `name: x` on its own line be
 * the same shape by the time anything looks at them.
 *
 * The parser is recursive descent over those lines, and it builds each
 * sequence and mapping on one shared stack rather than one allocation
 * per level: children are contiguous on the stack by the time a node is
 * finished, so finishing one is a single copy into the arena and a
 * truncation. Nesting is well nested, so one stack serves every depth.
 *
 * Everything a document hands back lives in an arena of a few large
 * chunks, which is why zy_free is one traversal of a short list and not
 * a walk of the tree.
 */
#include "yaml.h"

#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- the arena ---- */

/* Big enough that a case file of a few hundred kilobytes takes a few
 * dozen allocations, small enough that a one line document does not
 * take a page it will not use. */
#define ZY_CHUNK 8192

typedef struct zy_chunk {
    struct zy_chunk *next;
    size_t len;
    size_t cap;
    char data[];
} zy_chunk;

struct zy_doc {
    zy_chunk *chunks;
    zy_node root;
};

/* Aligned for anything this reader puts in an arena, which is nodes,
 * strings and arrays of both. */
#define ZY_ALIGN 8

static void *arena_alloc(zy_doc *doc, size_t size) {
    size_t need = (size + (ZY_ALIGN - 1)) & ~(size_t)(ZY_ALIGN - 1);
    zy_chunk *c = doc->chunks;
    void *p;
    if (c == NULL || c->cap - c->len < need) {
        size_t cap = need > ZY_CHUNK ? need : ZY_CHUNK;
        c = (zy_chunk *)malloc(sizeof *c + cap);
        if (c == NULL) {
            return NULL;
        }
        c->next = doc->chunks;
        c->len = 0;
        c->cap = cap;
        doc->chunks = c;
    }
    p = c->data + c->len;
    c->len += need;
    return p;
}

void zy_free(zy_doc *doc) {
    zy_chunk *c;
    if (doc == NULL) {
        return;
    }
    c = doc->chunks;
    while (c != NULL) {
        zy_chunk *next = c->next;
        free(c);
        c = next;
    }
    free(doc);
}

const zy_node *zy_root(const zy_doc *doc) { return doc == NULL ? NULL : &doc->root; }

/* ---- lines ---- */

/* One meaningful line: its indent, whether a `- ` opened it, what is
 * left after that, and where it was. The text points into the caller's
 * bytes, since a line lives only as long as the parse. */
typedef struct zy_line {
    size_t indent;
    int dash;
    const char *text;
    size_t len;
    size_t no;
} zy_line;

typedef struct zy_parser {
    zy_doc *doc;
    zy_line *lines;
    size_t count;
    size_t cap;
    /* the line the parser is looking at */
    size_t i;
    /* the shared stack the nodes of a level are built on */
    zy_node *stack;
    size_t stack_len;
    size_t stack_cap;
    zy_str *keys;
    size_t keys_len;
    size_t keys_cap;
    char *err;
    size_t err_len;
} zy_parser;

static int fail(zy_parser *p, const char *fmt, ...) {
    va_list ap;
    if (p->err != NULL && p->err_len > 0) {
        va_start(ap, fmt);
        vsnprintf(p->err, p->err_len, fmt, ap);
        va_end(ap);
    }
    return -1;
}

static int oom(zy_parser *p) { return fail(p, "out of memory"); }

/* Doubling, because the alternative is a count of lines taken in a pass
 * that would cost as much as the growth it saves. */
static int grow(void **items, size_t *cap, size_t need, size_t size) {
    size_t next = *cap == 0 ? 64 : *cap;
    void *bigger;
    if (need <= *cap) {
        return 0;
    }
    while (next < need) {
        next *= 2;
    }
    bigger = realloc(*items, next * size);
    if (bigger == NULL) {
        return -1;
    }
    *items = bigger;
    *cap = next;
    return 0;
}

static int push_line(zy_parser *p, size_t indent, int dash, const char *text, size_t len,
                     size_t no) {
    zy_line *line;
    if (grow((void **)&p->lines, &p->cap, p->count + 1, sizeof *p->lines) != 0) {
        return oom(p);
    }
    line = &p->lines[p->count++];
    line->indent = indent;
    line->dash = dash;
    line->text = text;
    line->len = len;
    line->no = no;
    return 0;
}

static int push_node(zy_parser *p, const zy_node *n) {
    if (grow((void **)&p->stack, &p->stack_cap, p->stack_len + 1, sizeof *p->stack) != 0) {
        return oom(p);
    }
    p->stack[p->stack_len++] = *n;
    return 0;
}

static int push_key(zy_parser *p, zy_str key) {
    if (grow((void **)&p->keys, &p->keys_cap, p->keys_len + 1, sizeof *p->keys) != 0) {
        return oom(p);
    }
    p->keys[p->keys_len++] = key;
    return 0;
}

/* A copy in the arena, NUL terminated, which is how every string a
 * document hands back gets there. */
static int arena_str(zy_parser *p, const char *s, size_t len, zy_str *out) {
    char *dst = (char *)arena_alloc(p->doc, len + 1);
    if (dst == NULL) {
        return oom(p);
    }
    memcpy(dst, s, len);
    dst[len] = '\0';
    out->ptr = dst;
    out->len = len;
    return 0;
}

static int is_space(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v';
}

static int is_key_char(char c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_' ||
           c == '-';
}

/* ---- comments and quotes ---- */

/* The offset of the quote that closes a run whose opening quote has
 * already been passed, or 0 when the line ends first.
 *
 * The two quoting styles hide a quote differently: a double quoted run
 * escapes with a backslash, and a single quoted run doubles the quote,
 * which is the only escape it has. */
static int closing_quote(const char *rest, size_t len, char quote, size_t *out) {
    size_t i = 0;
    while (i < len) {
        if (rest[i] == '\\' && quote == '"') {
            i += 2;
            continue;
        }
        if (rest[i] == quote) {
            if (quote == '\'' && i + 1 < len && rest[i + 1] == '\'') {
                i += 2;
                continue;
            }
            *out = i;
            return 1;
        }
        i++;
    }
    return 0;
}

/* How much of the line is content, which is everything up to an
 * unquoted `#`.
 *
 * Three rules keep this from eating content. A `#` starts a comment
 * only with whitespace before it, because one inside a word is part of
 * the word. A quote opens a quoted run only with whitespace before it,
 * because a quote inside a word is part of the word too, which is what
 * lets a `doc:` say "it's" without opening a run that never closes. And
 * a quote that opens nothing that closes was not a run at all, which is
 * what lets a `query:` hold `cast('  42  ' AS INT64)`: the second quote
 * has a space before it and so looks like an opening one, and nothing
 * after it closes.
 *
 * That last rule is why this cannot fail. A value that really does open
 * a quoted scalar and never close it is caught where the scalar is
 * read, because that is the only place the value's own start is known,
 * and the only place the difference matters. */
static size_t strip_comment(const char *line, size_t len) {
    size_t i = 0;
    while (i < len) {
        int opens = i == 0 || is_space(line[i - 1]);
        char c = line[i];
        size_t end = 0;
        if (c == '#' && opens) {
            return i;
        }
        if ((c == '"' || c == '\'') && opens && closing_quote(line + i + 1, len - i - 1, c, &end)) {
            i += 1 + end;
        }
        i++;
    }
    return len;
}

/* ---- what is not UTF-8 ---- */

/* The one place this reader can be handed something the Rust one cannot
 * be. That one takes a &str and so is given valid UTF-8 by its caller;
 * this one takes bytes off a disk. A file with a bad byte in it is
 * refused here, with a line number, rather than at the far end of the
 * runner where the C ABI refuses the string and cannot say which case
 * it came from. */
static int valid_utf8(const char *s, size_t len, size_t *bad) {
    size_t i = 0;
    while (i < len) {
        unsigned char c = (unsigned char)s[i];
        unsigned long cp;
        size_t extra, k;
        if (c < 0x80) {
            i++;
            continue;
        } else if ((c & 0xE0) == 0xC0) {
            extra = 1;
            cp = c & 0x1Fu;
        } else if ((c & 0xF0) == 0xE0) {
            extra = 2;
            cp = c & 0x0Fu;
        } else if ((c & 0xF8) == 0xF0) {
            extra = 3;
            cp = c & 0x07u;
        } else {
            *bad = i;
            return 0;
        }
        if (i + extra >= len) {
            *bad = i;
            return 0;
        }
        for (k = 1; k <= extra; k++) {
            unsigned char cc = (unsigned char)s[i + k];
            if ((cc & 0xC0) != 0x80) {
                *bad = i;
                return 0;
            }
            cp = (cp << 6) | (cc & 0x3Fu);
        }
        /* An overlong encoding, a surrogate half and a code point past
         * the last one are all bytes that decode and mean nothing. */
        if ((extra == 1 && cp < 0x80) || (extra == 2 && cp < 0x800) ||
            (extra == 3 && cp < 0x10000) || (cp >= 0xD800 && cp <= 0xDFFF) || cp > 0x10FFFF) {
            *bad = i;
            return 0;
        }
        i += extra + 1;
    }
    return 1;
}

/* ---- the lexer ---- */

static int lex(zy_parser *p, const char *src, size_t srclen) {
    size_t pos = 0;
    size_t no = 0;
    while (pos < srclen) {
        const char *raw = src + pos;
        const char *nl = (const char *)memchr(raw, '\n', srclen - pos);
        size_t rawlen = nl == NULL ? srclen - pos : (size_t)(nl - raw);
        const char *tab;
        const char *rest;
        size_t len, indent, rlen, bad = 0;
        int dash;

        pos += rawlen + (nl == NULL ? 0 : 1);
        no++;

        if (!valid_utf8(raw, rawlen, &bad)) {
            return fail(p, "line %lu: the byte at column %lu is not UTF-8, and these files are",
                        (unsigned long)no, (unsigned long)(bad + 1));
        }
        tab = (const char *)memchr(raw, '\t', rawlen);
        if (tab != NULL) {
            return fail(p, "line %lu: a tab at column %lu, and indentation here is spaces",
                        (unsigned long)no, (unsigned long)(tab - raw) + 1);
        }

        len = strip_comment(raw, rawlen);
        /* A trailing \r goes here, which is what makes a checkout with
         * CRLF line endings read the same as one without. */
        while (len > 0 && is_space(raw[len - 1])) {
            len--;
        }
        indent = 0;
        while (indent < len && is_space(raw[indent])) {
            indent++;
        }
        rest = raw + indent;
        rlen = len - indent;
        if (rlen == 0) {
            continue;
        }
        if (rlen == 3 && (memcmp(rest, "---", 3) == 0 || memcmp(rest, "...", 3) == 0)) {
            return fail(p, "line %lu: \"%.3s\" opens or closes a document, and a file here holds one",
                        (unsigned long)no, rest);
        }
        if (indent % 2 != 0) {
            return fail(p, "line %lu: indented %lu, and indentation here goes two spaces at a time",
                        (unsigned long)no, (unsigned long)indent);
        }

        dash = (rlen == 1 && rest[0] == '-') || (rlen >= 2 && rest[0] == '-' && rest[1] == ' ');
        if (!dash) {
            if (push_line(p, indent, 0, rest, rlen, no) != 0) {
                return -1;
            }
            continue;
        }
        rest++;
        rlen--;
        if (rlen >= 2 && rest[0] == ' ' && rest[1] == ' ') {
            return fail(p,
                        "line %lu: a `- ` takes exactly one space, so that what follows it lines "
                        "up with the lines under it",
                        (unsigned long)no);
        }
        while (rlen > 0 && is_space(rest[0])) {
            rest++;
            rlen--;
        }
        if (rlen >= 2 && rest[0] == '-' && rest[1] == ' ') {
            return fail(p,
                        "line %lu: a sequence opening straight into another one, which nothing "
                        "here needs",
                        (unsigned long)no);
        }
        if (push_line(p, indent, 1, NULL, 0, no) != 0) {
            return -1;
        }
        if (rlen > 0 && push_line(p, indent + 2, 0, rest, rlen, no) != 0) {
            return -1;
        }
    }
    return 0;
}

/* ---- the parser ---- */

/* The key and the rest of the line, when the line opens a mapping
 * entry. A key is a bare word, and the `:` after it ends the line or
 * has a space after it, so that a plain scalar holding a colon is still
 * a scalar. */
static int split_key(const char *text, size_t len, zy_str *key, zy_str *rest) {
    size_t i;
    size_t klen = 0;
    const char *r = text + len;
    size_t rlen = 0;
    int found = 0;
    for (i = 0; i + 1 < len; i++) {
        if (text[i] == ':' && text[i + 1] == ' ') {
            found = 1;
            break;
        }
    }
    if (found) {
        klen = i;
        r = text + i + 2;
        rlen = len - i - 2;
        while (rlen > 0 && is_space(r[0])) {
            r++;
            rlen--;
        }
    } else if (len > 0 && text[len - 1] == ':') {
        klen = len - 1;
    } else {
        return 0;
    }
    if (klen == 0) {
        return 0;
    }
    for (i = 0; i < klen; i++) {
        if (!is_key_char(text[i])) {
            return 0;
        }
    }
    key->ptr = text;
    key->len = klen;
    rest->ptr = r;
    rest->len = rlen;
    return 1;
}

static void blank(zy_node *n, zy_kind kind, size_t line) {
    n->kind = kind;
    n->quoted = 0;
    n->line = line;
    n->text.ptr = NULL;
    n->text.len = 0;
    n->keys = NULL;
    n->items = NULL;
    n->count = 0;
}

static int scalar(zy_parser *p, const char *text, size_t len, size_t line, zy_node *out) {
    int q;
    blank(out, ZY_SCALAR, line);
    for (q = 0; q < 2; q++) {
        char quote = q == 0 ? '"' : '\'';
        const char *body;
        char *dst;
        size_t blen, end = 0, i, n = 0;
        if (len == 0 || text[0] != quote) {
            continue;
        }
        body = text + 1;
        blen = len - 1;
        /* The closing quote is found by scanning rather than by taking
         * the last one on the line, so that `"a" and "b"` is refused
         * instead of read as one scalar with quotes in the middle. */
        if (!closing_quote(body, blen, quote, &end)) {
            return fail(p, "line %lu: a %c that opens and does not close on its line",
                        (unsigned long)line, quote);
        }
        if (end + 1 != blen) {
            return fail(p, "line %lu: \"%.*s\" after the scalar ends", (unsigned long)line,
                        (int)(blen - end - 1), body + end + 1);
        }
        /* Both forms only ever shrink, so the room the body takes is
         * room enough for what it unescapes to. */
        dst = (char *)arena_alloc(p->doc, end + 1);
        if (dst == NULL) {
            return oom(p);
        }
        for (i = 0; i < end; i++) {
            if (quote == '\'') {
                /* One escape, the doubled quote, and a backslash in it
                 * is a backslash. */
                dst[n++] = body[i];
                if (body[i] == '\'' && i + 1 < end && body[i + 1] == '\'') {
                    i++;
                }
                continue;
            }
            if (body[i] != '\\') {
                dst[n++] = body[i];
                continue;
            }
            if (i + 1 >= end) {
                return fail(p, "line %lu: a scalar ending in a backslash", (unsigned long)line);
            }
            switch (body[++i]) {
            case '"': dst[n++] = '"'; break;
            case '\\': dst[n++] = '\\'; break;
            case 'n': dst[n++] = '\n'; break;
            case 'r': dst[n++] = '\r'; break;
            case 't': dst[n++] = '\t'; break;
            case 'b': dst[n++] = '\b'; break;
            case 'f': dst[n++] = '\f'; break;
            case '0': dst[n++] = '\0'; break;
            default:
                return fail(p, "line %lu: \\%c is not an escape", (unsigned long)line, body[i]);
            }
        }
        dst[n] = '\0';
        out->quoted = 1;
        out->text.ptr = dst;
        out->text.len = n;
        return 0;
    }
    if (len > 0 && text[0] != '\0' && strchr("[]{}&*!|>%@`", text[0]) != NULL) {
        return fail(p,
                    "line %lu: a plain scalar opening with '%c', which is a construct this reader "
                    "does not read",
                    (unsigned long)line, text[0]);
    }
    return arena_str(p, text, len, &out->text);
}

static int node(zy_parser *p, size_t indent, zy_node *out);

/* The children of a level, taken off the shared stack and put in the
 * arena where they will outlive it. */
static int harvest(zy_parser *p, size_t base, size_t kbase, zy_node *out) {
    size_t count = p->stack_len - base;
    if (count > 0) {
        zy_node *items = (zy_node *)arena_alloc(p->doc, count * sizeof *items);
        if (items == NULL) {
            return oom(p);
        }
        memcpy(items, p->stack + base, count * sizeof *items);
        out->items = items;
        if (out->kind == ZY_MAP) {
            zy_str *keys = (zy_str *)arena_alloc(p->doc, count * sizeof *keys);
            if (keys == NULL) {
                return oom(p);
            }
            memcpy(keys, p->keys + kbase, count * sizeof *keys);
            out->keys = keys;
        }
    }
    out->count = count;
    p->stack_len = base;
    p->keys_len = kbase;
    return 0;
}

static int seq(zy_parser *p, size_t indent, zy_node *out) {
    size_t base = p->stack_len;
    size_t kbase = p->keys_len;
    blank(out, ZY_SEQ, p->lines[p->i].no);
    while (p->i < p->count && p->lines[p->i].dash && p->lines[p->i].indent == indent) {
        size_t at = p->lines[p->i].no;
        zy_node item;
        p->i++;
        if (p->i < p->count && p->lines[p->i].indent == indent + 2) {
            if (node(p, indent + 2, &item) != 0) {
                return -1;
            }
        } else if (p->i < p->count && p->lines[p->i].indent > indent) {
            return fail(p,
                        "line %lu: indented %lu, where an item of the sequence on line %lu is "
                        "indented %lu",
                        (unsigned long)p->lines[p->i].no, (unsigned long)p->lines[p->i].indent,
                        (unsigned long)at, (unsigned long)(indent + 2));
        } else {
            return fail(p, "line %lu: a `-` with nothing after it", (unsigned long)at);
        }
        if (push_node(p, &item) != 0) {
            return -1;
        }
    }
    return harvest(p, base, kbase, out);
}

static int map(zy_parser *p, size_t indent, zy_node *out) {
    size_t base = p->stack_len;
    size_t kbase = p->keys_len;
    zy_str key, rest, owned;
    blank(out, ZY_MAP, p->lines[p->i].no);
    while (p->i < p->count && !p->lines[p->i].dash && p->lines[p->i].indent == indent &&
           split_key(p->lines[p->i].text, p->lines[p->i].len, &key, &rest)) {
        size_t at = p->lines[p->i].no;
        size_t k;
        zy_node value;
        p->i++;
        if (rest.len > 0) {
            if (scalar(p, rest.ptr, rest.len, at, &value) != 0) {
                return -1;
            }
        } else if (p->i < p->count && p->lines[p->i].indent == indent + 2) {
            if (node(p, indent + 2, &value) != 0) {
                return -1;
            }
        } else if (p->i < p->count && p->lines[p->i].indent > indent) {
            return fail(p,
                        "line %lu: indented %lu, where what is under `%.*s:` on line %lu is "
                        "indented %lu",
                        (unsigned long)p->lines[p->i].no, (unsigned long)p->lines[p->i].indent,
                        (int)key.len, key.ptr, (unsigned long)at, (unsigned long)(indent + 2));
        } else {
            blank(&value, ZY_EMPTY, at);
        }
        for (k = kbase; k < p->keys_len; k++) {
            if (p->keys[k].len == key.len && memcmp(p->keys[k].ptr, key.ptr, key.len) == 0) {
                return fail(p, "line %lu: %.*s is set twice in one mapping", (unsigned long)at,
                            (int)key.len, key.ptr);
            }
        }
        if (arena_str(p, key.ptr, key.len, &owned) != 0 || push_key(p, owned) != 0 ||
            push_node(p, &value) != 0) {
            return -1;
        }
    }
    return harvest(p, base, kbase, out);
}

/* The node that starts at p->i and is indented indent, leaving p->i on
 * the first line that is not part of it. */
static int node(zy_parser *p, size_t indent, zy_node *out) {
    const zy_line *line = &p->lines[p->i];
    zy_str key, rest;
    if (line->dash) {
        return seq(p, indent, out);
    }
    /* A mapping key is a bare word and a `:`. Anything else at this
     * position is a scalar standing on its own, which is what the items
     * of a sequence of scalars are. */
    if (split_key(line->text, line->len, &key, &rest)) {
        return map(p, indent, out);
    }
    p->i++;
    return scalar(p, line->text, line->len, line->no, out);
}

zy_doc *zy_parse(const char *text, size_t len, char *err, size_t err_len) {
    zy_parser p;
    zy_doc *doc;
    memset(&p, 0, sizeof p);
    p.err = err;
    p.err_len = err_len;
    if (err != NULL && err_len > 0) {
        err[0] = '\0';
    }
    doc = (zy_doc *)calloc(1, sizeof *doc);
    if (doc == NULL) {
        oom(&p);
        return NULL;
    }
    p.doc = doc;
    if (lex(&p, text, len) != 0) {
        goto stop;
    }
    if (p.count == 0) {
        fail(&p, "the file has nothing in it");
        goto stop;
    }
    if (p.lines[0].indent != 0) {
        fail(&p, "line %lu: the first line is indented", (unsigned long)p.lines[0].no);
        goto stop;
    }
    if (node(&p, 0, &doc->root) != 0) {
        goto stop;
    }
    if (p.i != p.count) {
        fail(&p, "line %lu: this belongs to nothing above it", (unsigned long)p.lines[p.i].no);
        goto stop;
    }
    free(p.lines);
    free(p.stack);
    free(p.keys);
    return doc;

stop:
    free(p.lines);
    free(p.stack);
    free(p.keys);
    zy_free(doc);
    return NULL;
}

/* An error about the file rather than about what is in it. Separate
 * from fail because there is no parser yet when a file will not open,
 * and NULL tolerant for the same reason zy_parse is: a caller that does
 * not want the message should not have to find somewhere to put it. */
static void note(char *err, size_t err_len, const char *fmt, ...) {
    va_list ap;
    if (err == NULL || err_len == 0) {
        return;
    }
    va_start(ap, fmt);
    vsnprintf(err, err_len, fmt, ap);
    va_end(ap);
}

zy_doc *zy_parse_file(const char *path, char *err, size_t err_len) {
    FILE *f = fopen(path, "rb");
    char *buf = NULL;
    size_t cap = 1 << 16;
    size_t len = 0;
    zy_doc *doc;
    if (f == NULL) {
        note(err, err_len, "%s: %s", path, strerror(errno));
        return NULL;
    }
    buf = (char *)malloc(cap);
    if (buf == NULL) {
        fclose(f);
        note(err, err_len, "%s: out of memory", path);
        return NULL;
    }
    for (;;) {
        size_t n = fread(buf + len, 1, cap - len, f);
        char *bigger;
        len += n;
        if (len < cap) {
            break;
        }
        cap *= 2;
        bigger = (char *)realloc(buf, cap);
        if (bigger == NULL) {
            free(buf);
            fclose(f);
            note(err, err_len, "%s: out of memory", path);
            return NULL;
        }
        buf = bigger;
    }
    if (ferror(f)) {
        note(err, err_len, "%s: %s", path, strerror(errno));
        fclose(f);
        free(buf);
        return NULL;
    }
    fclose(f);
    doc = zy_parse(buf, len, err, err_len);
    free(buf);
    return doc;
}

/* ---- reading a document ---- */

const char *zy_kind_name(const zy_node *n) {
    if (n == NULL) {
        return "nothing";
    }
    switch (n->kind) {
    case ZY_SCALAR: return "a scalar";
    case ZY_SEQ: return "a sequence";
    case ZY_MAP: return "a mapping";
    default: return "nothing";
    }
}

const zy_node *zy_get(const zy_node *n, const char *key) {
    size_t i, len;
    if (n == NULL || n->kind != ZY_MAP) {
        return NULL;
    }
    len = strlen(key);
    for (i = 0; i < n->count; i++) {
        if (n->keys[i].len == len && memcmp(n->keys[i].ptr, key, len) == 0) {
            return &n->items[i];
        }
    }
    return NULL;
}

zy_str zy_text(const zy_node *n) {
    zy_str none;
    none.ptr = NULL;
    none.len = 0;
    if (n == NULL || n->kind != ZY_SCALAR) {
        return none;
    }
    return n->text;
}

int zy_eq(zy_str s, const char *lit) {
    size_t len;
    if (s.ptr == NULL || lit == NULL) {
        return 0;
    }
    len = strlen(lit);
    return s.len == len && memcmp(s.ptr, lit, len) == 0;
}

int zy_seq_or_empty(const zy_node *n, const zy_node **items, size_t *count) {
    *items = NULL;
    *count = 0;
    if (n == NULL) {
        return -1;
    }
    if (n->kind == ZY_EMPTY) {
        return 0;
    }
    if (n->kind == ZY_SEQ) {
        *items = n->items;
        *count = n->count;
        return 0;
    }
    return -1;
}

zy_str zy_unknown(const zy_node *n, const char *const *known, size_t known_len) {
    zy_str none;
    size_t i, k;
    none.ptr = NULL;
    none.len = 0;
    if (n == NULL || n->kind != ZY_MAP) {
        return none;
    }
    for (i = 0; i < n->count; i++) {
        int seen = 0;
        for (k = 0; k < known_len && !seen; k++) {
            seen = zy_eq(n->keys[i], known[k]);
        }
        if (!seen) {
            return n->keys[i];
        }
    }
    return none;
}
