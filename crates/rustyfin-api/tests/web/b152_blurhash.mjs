// B152: BlurHash previews. Runs app.js's decodeBlurhash in a vm sandbox.
// Usage: node b152_blurhash.mjs PATH/TO/app.js (run by api_tests.rs b152_blurhash_decoder).
import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = fs.readFileSync(process.argv[2], 'utf8');
const HEAD = 'function decodeBlurhash(hash, w, h) {';
const start = source.indexOf(HEAD);
assert.ok(start >= 0, `app.js has ${HEAD}`);
const end = source.indexOf('\n}\n', start) + 3;
const digitsLine = source.split('\n').find(l => l.startsWith('const BLURHASH_DIGITS ='));
assert.ok(digitsLine, 'app.js has BLURHASH_DIGITS');
const context = vm.createContext({});
vm.runInContext(`${digitsLine.replace('const ', 'var ')}\n${source.slice(start, end)}`, context);
const decode = (...args) => context.decodeBlurhash(...args);

const DIGITS = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~';
const base83 = (n, len) => Array.from({ length: len }, (_, i) => DIGITS[Math.floor(n / 83 ** (len - 1 - i)) % 83]).join('');

test('a one-component hash decodes to its flat colour', () => {
    for (const [r, g, b] of [[255, 0, 0], [12, 200, 99], [0, 0, 0]]) {
        const hash = '00' + base83((r << 16) | (g << 8) | b, 4);
        const px = decode(hash, 4, 6);
        assert.equal(px.length, 4 * 6 * 4);
        for (let o = 0; o < px.length; o += 4) {
            assert.deepEqual([px[o], px[o + 1], px[o + 2], px[o + 3]], [r, g, b, 255], hash);
        }
    }
});

test('the reference hash decodes to opaque pixels that vary across the picture', () => {
    const px = decode('LEHV6nWB2yk8pyo0adR*.7kCMdnj', 20, 30);
    assert.equal(px.length, 20 * 30 * 4);
    for (let o = 3; o < px.length; o += 4) assert.equal(px[o], 255);
    const first = Array.from(px.slice(0, 3));
    const last = Array.from(px.slice(-4, -1));
    assert.notDeepEqual(first, last, 'a 4x3 hash is not flat');
});

test('the average colour is the hash DC component', () => {
    const start = source.indexOf('function blurhashAverage(hash) {');
    assert.ok(start >= 0, 'app.js has blurhashAverage');
    vm.runInContext(source.slice(start, source.indexOf('\n}\n', start) + 3), context);
    assert.equal(context.blurhashAverage('00' + base83((12 << 16) | (200 << 8) | 99, 4)), 'rgb(12, 200, 99)');
    // A many-component hash: its DC digits (2-5) alone, whatever follows
    assert.equal(context.blurhashAverage('L0' + base83(0x102030, 4) + 'WB2yk8pyo0adR*.7kCMdnj'), 'rgb(16, 32, 48)');
    for (const bad of [null, 'L', '00~~~~', '00!AAA']) assert.equal(context.blurhashAverage(bad), null, String(bad));
});

test('a malformed hash gives null', () => {
    for (const hash of [null, undefined, '', 'L', 'LEHV6nWB2yk8pyo0adR*.7kCMdn', 'LEHV6nWB2yk8pyo0adR*.7kCMdnj!', '00"AAAA']) {
        assert.equal(decode(hash, 4, 4), null, String(hash));
    }
});
