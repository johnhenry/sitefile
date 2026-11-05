import { test } from 'node:test';
import assert from 'node:assert/strict';
import { renderMarkdown } from '../src/markdown.js';

const html = (s: string): string => renderMarkdown(s).html;
const eq = (src: string, expected: string): void => assert.equal(html(src), expected);

test('empty input', () => {
  assert.deepEqual(renderMarkdown(''), { html: '', excerpt: '' });
  assert.deepEqual(renderMarkdown('\n\n  \n'), { html: '', excerpt: '' });
});

test('paragraphs and blank lines', () => {
  eq('one\ntwo\n\nthree', '<p>one\ntwo</p>\n<p>three</p>\n');
});

test('CRLF and CR line endings', () => {
  eq('a\r\nb\r\n\r\nc', '<p>a\nb</p>\n<p>c</p>\n');
  eq('# T\r\n\r\ntext', '<h1>T</h1>\n<p>text</p>\n');
});

test('ATX headings', () => {
  eq('# a\n## b ##\n###### f\n####### no', '<h1>a</h1>\n<h2>b</h2>\n<h6>f</h6>\n<p>####### no</p>\n');
  eq('#\n#nospace\n# *x* #', '<h1></h1>\n<p>#nospace</p>\n<h1><em>x</em></h1>\n');
  eq('# a # b', '<h1>a # b</h1>\n');
});

test('setext headings', () => {
  eq('Title\n=====\n\nSub\n---\n', '<h1>Title</h1>\n<h2>Sub</h2>\n');
  eq('multi\nline\n===', '<h1>multi\nline</h1>\n');
});

test('hard breaks', () => {
  eq('a  \nb', '<p>a<br>\nb</p>\n');
  eq('a\\\nb', '<p>a<br>\nb</p>\n');
  eq('a \nb', '<p>a\nb</p>\n');
  eq('end  ', '<p>end</p>\n');
  eq('end\\', '<p>end\\</p>\n');
});

test('thematic breaks', () => {
  eq('---\n\n***\n\n___\n\n- - -', '<hr>\n<hr>\n<hr>\n<hr>\n');
  eq('a\n\n--', '<p>a</p>\n<p>--</p>\n');
});

test('fenced code', () => {
  eq('```js\nlet a = <b> & "c";\n\nx\n```', '<pre><code class="language-js">let a = &lt;b&gt; &amp; &quot;c&quot;;\n\nx\n</code></pre>\n');
  eq('~~~\n*not em*\n~~~', '<pre><code>*not em*\n</code></pre>\n');
  eq('````\n```\n````', '<pre><code>```\n</code></pre>\n');
  eq('```\n```', '<pre><code></code></pre>\n');
});

test('fence info string: class only for safe words', () => {
  eq('```c++ extra\nx\n```', '<pre><code class="language-c++">x\n</code></pre>\n');
  eq('```a"b\nx\n```', '<pre><code>x\n</code></pre>\n');
  eq('```<script>\nx\n```', '<pre><code>x\n</code></pre>\n');
});

test('unterminated fence runs to end of document', () => {
  eq('```\nabc\n\ndef', '<pre><code>abc\n\ndef\n</code></pre>\n');
  eq('> ```\n> x', '<blockquote>\n<pre><code>x\n</code></pre>\n</blockquote>\n');
});

test('indented code', () => {
  eq('    a\n\n    b\n\npara', '<pre><code>a\n\nb\n</code></pre>\n<p>para</p>\n');
  eq('para\n    cont', '<p>para\ncont</p>\n');
  eq('\ttabbed', '<pre><code>tabbed\n</code></pre>\n');
});

test('blockquotes', () => {
  eq('> a\n> b', '<blockquote>\n<p>a\nb</p>\n</blockquote>\n');
  eq('> a\nlazy', '<blockquote>\n<p>a\nlazy</p>\n</blockquote>\n');
  eq('> # h\n>\n> - x\n>> deep', '<blockquote>\n<h1>h</h1>\n<ul>\n<li>x</li>\n</ul>\n<blockquote>\n<p>deep</p>\n</blockquote>\n</blockquote>\n');
  eq('> a\n\n> b', '<blockquote>\n<p>a</p>\n</blockquote>\n<blockquote>\n<p>b</p>\n</blockquote>\n');
});

test('tight unordered lists with all markers', () => {
  eq('- a\n- b', '<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n');
  eq('* a\n* b', '<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n');
  eq('+ a', '<ul>\n<li>a</li>\n</ul>\n');
});

test('changing the bullet starts a new list', () => {
  eq('- a\n* b', '<ul>\n<li>a</li>\n</ul>\n<ul>\n<li>b</li>\n</ul>\n');
});

test('ordered lists and start', () => {
  eq('1. a\n2. b', '<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n');
  eq('1) a\n2) b', '<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n');
  eq('3. a\n4. b', '<ol start="3">\n<li>a</li>\n<li>b</li>\n</ol>\n');
  eq('0. a', '<ol start="0">\n<li>a</li>\n</ol>\n');
});

test('loose lists', () => {
  eq('- a\n\n- b', '<ul>\n<li>\n<p>a</p>\n</li>\n<li>\n<p>b</p>\n</li>\n</ul>\n');
  eq('- a\n\n  b\n- c', '<ul>\n<li>\n<p>a</p>\n<p>b</p>\n</li>\n<li>\n<p>c</p>\n</li>\n</ul>\n');
});

test('nested lists', () => {
  eq('- a\n  - b\n    - c\n- d', '<ul>\n<li>a\n<ul>\n<li>b\n<ul>\n<li>c</li>\n</ul>\n</li>\n</ul>\n</li>\n<li>d</li>\n</ul>\n');
  eq('1. a\n   - x\n2. b', '<ol>\n<li>a\n<ul>\n<li>x</li>\n</ul>\n</li>\n<li>b</li>\n</ol>\n');
});

test('blank line inside a nested list does not loosen the outer list', () => {
  const out = html('- a\n  - b\n\n  - c\n- d');
  assert.match(out, /<li>a\n<ul>\n<li>\n<p>b<\/p>/);
  assert.match(out, /<li>d<\/li>/);
});

test('multi-line items, lazy continuation, code in item', () => {
  eq('- one\n  two\n- three', '<ul>\n<li>one\ntwo</li>\n<li>three</li>\n</ul>\n');
  eq('- one\ntwo', '<ul>\n<li>one\ntwo</li>\n</ul>\n');
  eq('- ```\n  x\n  ```', '<ul>\n<li>\n<pre><code>x\n</code></pre>\n</li>\n</ul>\n');
});

test('list ends at a paragraph after a blank line', () => {
  eq('- a\n\npara', '<ul>\n<li>a</li>\n</ul>\n<p>para</p>\n');
});

test('a list can interrupt a paragraph, but 2. cannot', () => {
  eq('text\n- a', '<p>text</p>\n<ul>\n<li>a</li>\n</ul>\n');
  eq('text\n2. a', '<p>text\n2. a</p>\n');
  eq('text\n1. a', '<p>text</p>\n<ol>\n<li>a</li>\n</ol>\n');
});

test('empty list items', () => {
  eq('-\n- b', '<ul>\n<li></li>\n<li>b</li>\n</ul>\n');
});

test('inline: emphasis, strong, strikethrough', () => {
  eq('*a* _b_ **c** __d__ ~~e~~', '<p><em>a</em> <em>b</em> <strong>c</strong> <strong>d</strong> <del>e</del></p>\n');
  eq('***both***', '<p><em><strong>both</strong></em></p>\n');
  eq('**a *b* c**', '<p><strong>a <em>b</em> c</strong></p>\n');
  eq('a*b*c', '<p>a<em>b</em>c</p>\n');
});

test('inline: intraword underscore is literal', () => {
  eq('snake_case_word and 2*3*4', '<p>snake_case_word and 2<em>3</em>4</p>\n');
  eq('foo_bar_', '<p>foo_bar_</p>\n');
});

test('inline: unclosed emphasis stays literal', () => {
  eq('*a', '<p>*a</p>\n');
  eq('**a *b', '<p>**a *b</p>\n');
  eq('a * b * c', '<p>a * b * c</p>\n');
  eq('~single~ ~~~triple~~~', '<p>~single~ ~~~triple~~~</p>\n');
  eq('~~open', '<p>~~open</p>\n');
});

test('inline: backslash escapes', () => {
  eq('\\*not em\\* \\# \\\\ \\a', '<p>*not em* # \\ \\a</p>\n');
  eq('\\<b\\>', '<p>&lt;b&gt;</p>\n');
});

test('inline: code spans', () => {
  eq('`a *b* <i>` ``x ` y``', '<p><code>a *b* &lt;i&gt;</code> <code>x ` y</code></p>\n');
  eq('` a `', '<p><code>a</code></p>\n');
  eq('`unclosed', '<p>`unclosed</p>\n');
  eq('`a\nb`', '<p><code>a b</code></p>\n');
});

test('links', () => {
  eq('[t](http://x.com)', '<p><a href="http://x.com">t</a></p>\n');
  eq('[t](/a/b "T \\"q\\"")', '<p><a href="/a/b" title="T &quot;q&quot;">t</a></p>\n');
  eq("[*t*](rel.html 'x')", '<p><a href="rel.html" title="x"><em>t</em></a></p>\n');
  eq('[t](#frag) [m](mailto:a@b.c) [p](http://x/(a))', '<p><a href="#frag">t</a> <a href="mailto:a@b.c">m</a> <a href="http://x/(a)">p</a></p>\n');
  eq('[t](<a b.html>)', '<p><a href="a%20b.html">t</a></p>\n');
  eq('[t]()', '<p><a href="">t</a></p>\n');
});

test('malformed links stay literal', () => {
  eq('[t] (x) [u](x', '<p>[t] (x) [u](x</p>\n');
  eq('[unclosed(x)', '<p>[unclosed(x)</p>\n');
});

test('images', () => {
  eq('![a *b*](/i.png "T")', '<p><img src="/i.png" alt="a b" title="T"></p>\n');
  eq('![](x.png)', '<p><img src="x.png" alt=""></p>\n');
  eq('![a"b](x.png)', '<p><img src="x.png" alt="a&quot;b"></p>\n');
});

test('autolinks', () => {
  eq('<https://example.com/a?b=1&c=2>', '<p><a href="https://example.com/a?b=1&amp;c=2">https://example.com/a?b=1&amp;c=2</a></p>\n');
  eq('<me@example.com>', '<p><a href="mailto:me@example.com">me@example.com</a></p>\n');
  eq('a < b > c', '<p>a &lt; b &gt; c</p>\n');
});

test('text escaping', () => {
  eq('a & b < c > "d" \'e\'', '<p>a &amp; b &lt; c &gt; &quot;d&quot; &#39;e&#39;</p>\n');
  eq('&amp; &lt;', '<p>&amp;amp; &amp;lt;</p>\n');
});

// ---------------------------------------------------------------- safety

test('raw HTML is escaped, never passed through', () => {
  eq('<script>alert(1)</script>', '<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>\n');
  eq('<div onclick="x">hi</div>', '<p>&lt;div onclick=&quot;x&quot;&gt;hi&lt;/div&gt;</p>\n');
  eq('*a* <img src=x onerror=alert(1)>', '<p><em>a</em> &lt;img src=x onerror=alert(1)&gt;</p>\n');
  eq('# <b>x</b>', '<h1>&lt;b&gt;x&lt;/b&gt;</h1>\n');
  eq('> <iframe>', '<blockquote>\n<p>&lt;iframe&gt;</p>\n</blockquote>\n');
  eq('<!-- c -->', '<p>&lt;!-- c --&gt;</p>\n');
});

test('dangerous link schemes are neutralized', () => {
  const bad = [
    '[x](javascript:alert(1))',
    '[x](JaVaScRiPt:alert(1))',
    '[x](<java\tscript:alert(1)>)',
    '[x](<  javascript:alert(1)>)',
    '[x](data:text/html;base64,AAAA)',
    '[x](vbscript:msgbox)',
    '[x](file:///etc/passwd)',
    '[x](<\u0001javascript:alert(1)>)',
  ];
  for (const src of bad) {
    const out = html(src);
    assert.ok(!out.includes('<a'), `${src} -> ${out}`);
    assert.ok(!/href/i.test(out), `${src} -> ${out}`);
  }
  eq('[click *me*](javascript:alert(1))', '<p>click <em>me</em></p>\n');
});

test('entity-encoded schemes stay inert because & is always escaped', () => {
  eq('[x](<jav&#x09;ascript:alert(1)>)', '<p><a href="jav&amp;#x09;ascript:alert(1)">x</a></p>\n');
});

test('dangerous image and autolink schemes are neutralized', () => {
  eq('![alt text](javascript:alert(1))', '<p>alt text</p>\n');
  eq('![a](data:image/svg+xml;base64,AAAA)', '<p>a</p>\n');
  eq('<javascript:alert(1)>', '<p>&lt;javascript:alert(1)&gt;</p>\n');
  eq('<data:text/html,x>', '<p>&lt;data:text/html,x&gt;</p>\n');
});

test('attribute values are quoted and escaped', () => {
  eq('[t](/a"onmouseover="x)', '<p><a href="/a&quot;onmouseover=&quot;x">t</a></p>\n');
  eq('[t](<a b"c> "ti\'tle")', '<p><a href="a%20b&quot;c" title="ti&#39;tle">t</a></p>\n');
  eq("![a'b](x.png 'q\"r')", '<p><img src="x.png" alt="a&#39;b" title="q&quot;r"></p>\n');
});

test('relative and absolute URLs are allowed', () => {
  eq('[a](../x/y.html) [b](/abs) [c](?q=a:b) [d](x/y:z)', '<p><a href="../x/y.html">a</a> <a href="/abs">b</a> <a href="?q=a:b">c</a> <a href="x/y:z">d</a></p>\n');
});

test('deep nesting does not overflow the stack', () => {
  const src = '> '.repeat(500) + 'x';
  assert.match(html(src), /x/);
  const nested = html('['.repeat(500) + 'x' + '](y)'.repeat(500));
  assert.ok(nested.length > 0);
});

test('output is deterministic', () => {
  const src = '# T\n\n- a\n- b\n\n*x* [l](/u)';
  assert.equal(html(src), html(src));
});

// --------------------------------------------------------------- excerpt

test('excerpt: first paragraph as plain text', () => {
  const r = renderMarkdown('# Title\n\nHello **bold** and `code` with [a link](/x)  \nand <b>raw</b>.\n\nSecond.');
  assert.equal(r.excerpt, 'Hello bold and code with a link and <b>raw</b>.');
});

test('excerpt: image alt, whitespace collapsed, empty cases', () => {
  assert.equal(renderMarkdown('a   ![pic](x.png)\n\tb').excerpt, 'a pic b');
  assert.equal(renderMarkdown('# Only heading').excerpt, '');
  assert.equal(renderMarkdown('').excerpt, '');
  assert.equal(renderMarkdown('```\ncode\n```').excerpt, '');
});

test('excerpt: short text is untouched', () => {
  assert.equal(renderMarkdown('x'.repeat(200)).excerpt, 'x'.repeat(200));
});

test('excerpt: truncates at a word boundary with an ellipsis, max 200 chars', () => {
  const words = Array.from({ length: 60 }, (_, i) => `word${i}`).join(' ');
  const ex = renderMarkdown(words).excerpt;
  assert.ok(ex.endsWith('…'));
  assert.ok(ex.length <= 200, String(ex.length));
  const body = ex.slice(0, -1);
  assert.ok(words.startsWith(body));
  assert.ok(/^word\d+$/.test(body.split(' ').pop() ?? ''), 'ends on a whole word');
  assert.notEqual(words[body.length], undefined);
  assert.equal(words[body.length], ' ');
});

test('excerpt: a single long word is hard cut', () => {
  const ex = renderMarkdown('y'.repeat(500)).excerpt;
  assert.equal(ex, 'y'.repeat(199) + '…');
});
