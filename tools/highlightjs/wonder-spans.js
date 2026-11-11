// Loaded after a highlight.js bundle. Exposes wonderSpans(code, lang) ->
// "start,len,role,..." (UTF-16 offsets; roles index Wonder's SyntaxRole).
(function () {
  const Base = hljs.highlight('x', { language: 'plaintext' })._emitter.constructor;
  class NoHTML extends Base { toHTML() { return ''; } }
  globalThis.__wonderNoHTML = NoHTML;
  globalThis.__wonderBaseEmitter = Base;
  const K = 0, S = 1, C = 2, N = 3, T = 4, A = 5, F = 6, P = 7, PLAIN = -1, INHERIT = -2;
  const KEYLANG = new Set(['json', 'jsonc', 'yaml', 'yml', 'ini', 'toml', 'properties']);
  const map = {
    keyword: K, 'meta.keyword': K, literal: K, 'variable.language': K, section: K, name: K,
    'selector-tag': K, 'selector-pseudo': K, doctag: K, 'template-tag': K,
    string: S, regexp: S, 'char.escape': S, symbol: S, 'meta.string': S, code: S, link: S, formula: S,
    comment: C, quote: C,
    number: N,
    type: T, built_in: T, 'title.class': T, 'title.class.inherited': T, class: T,
    'selector-class': T, 'selector-id': T,
    attribute: A, meta: A, 'selector-attr': A,
    'title.function': F, 'title.function.invoke': F, title: F,
    punctuation: P, operator: P, bullet: P,
    subst: PLAIN,
  };
  function roleFor(scope, lang) {
    if (scope === 'attr') return KEYLANG.has(lang) ? T : A;
    if (scope in map) return map[scope];
    const head = scope.split('.')[0];
    if (head === 'attr') return KEYLANG.has(lang) ? T : A;
    return head in map ? map[head] : INHERIT;
  }
  globalThis.wonderSpans = function (code, lang) {
    const r = hljs.highlight(code, { language: lang, ignoreIllegals: true });
    const out = [];
    let pos = 0;
    (function walk(node, role, l) {
      for (const c of node.children) {
        if (typeof c === 'string') {
          if (role >= 0) out.push(pos, c.length, role);
          pos += c.length;
        } else {
          let nl = l, nr = role;
          if (c.scope && c.scope.startsWith('language:')) { nl = c.scope.slice(9); nr = PLAIN; }
          else if (c.scope) { const m = roleFor(c.scope, l); if (m !== INHERIT) nr = m; }
          walk(c, nr, nl);
        }
      }
    })(r._emitter.rootNode, PLAIN, r.language);
    return out.join(',');
  };
  // Whether a fence tag names a language here; plain text stays plain.
  globalThis.wonderSupports = function (tag) {
    const language = hljs.getLanguage(tag);
    return Boolean(language) && language !== hljs.getLanguage('plaintext');
  };
  globalThis.wonderUseHTML = function (on) {
    hljs.configure({ __emitter: on ? Base : NoHTML });
  };
  wonderUseHTML(false);
})();
