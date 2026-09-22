const assert = require('node:assert/strict');
const fs = require('node:fs');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync('src/adapters/ghostty_first_terminal.js', 'utf8');
const code = text => [...text].reduce((value, character) => value * 256 + character.charCodeAt(0), 0);

function lookup({apps = 1, surfaces = 1, tty = '/dev/ttys123', failure = 0, timeout = false} = {}) {
    const events = [];
    const descriptor = {
        get recordDescriptor() {
            return {
                fields: {},
                setDescriptorForKeyword(value, key) { this.fields[key] = value; },
                coerceToDescriptorType(type) {
                    assert.equal(type, code('obj '));
                    return this;
                }
            };
        },
        descriptorWithTypeCode: value => value,
        descriptorWithEnumCode: value => value,
        descriptorWithDescriptorTypeData: (type, data) => ({type, data}),
        descriptorWithProcessIdentifier: pid => ({pid}),
        nullDescriptor: null,
        appleEventWithEventClassEventIDTargetDescriptorReturnIDTransactionID(eventClass, eventId, target) {
            assert.equal(eventClass, code('core'));
            assert.equal(eventId, code('getd'));
            assert.deepEqual(target, {pid: 42});
            return {
                setParamDescriptorForKeyword(object, keyword) {
                    assert.equal(keyword, code('----'));
                    this.object = object;
                },
                sendEventWithOptionsTimeoutError(flags, seconds) {
                    assert.equal(flags, 0x03 | 0x10 | 0x80);
                    assert.equal(seconds, 0.5);
                    events.push(this.object);
                    if (timeout) return null;
                    const isTerminalList = this.object.fields[code('want')] === code('Gtrm');
                    if (!isTerminalList) assert.equal(this.object.fields[code('seld')], code('Gtty'));
                    return {
                        isNil: () => false,
                        paramDescriptorForKeyword: key => key === code('errn')
                            ? {isNil: () => false, int32Value: failure}
                            : isTerminalList
                                ? {numberOfItems: surfaces, descriptorAtIndex: () => ({surface: true})}
                                : {stringValue: tty}
                    };
                }
            };
        }
    };
    const context = {
        ObjC: {import() {}, unwrap: value => value},
        Ref: () => [],
        $: {
            NSAppleEventDescriptor: descriptor,
            NSRunningApplication: {
                runningApplicationsWithBundleIdentifier(bundle) {
                    assert.equal(bundle, 'com.mitchellh.ghostty');
                    return {count: apps, objectAtIndex: () => ({processIdentifier: 42})};
                }
            }
        }
    };
    vm.runInNewContext(script, context);
    return {accepted: context.run(['/dev/ttys123']), events};
}

test('only the single matching surface is reusable', () => {
    assert.equal(lookup().accepted, true);
    assert.equal(lookup({tty: '/dev/ttys999'}).accepted, false);
    assert.equal(lookup({surfaces: 0}).accepted, false);
    assert.equal(lookup({surfaces: 2}).accepted, false);
});

test('absent or multiple Ghostty instances are never contacted', () => {
    for (const apps of [0, 2]) {
        const result = lookup({apps});
        assert.equal(result.accepted, false);
        assert.equal(result.events.length, 0);
    }
});

test('a disappearing or unresponsive terminal fails without reconnecting', () => {
    assert.throws(() => lookup({timeout: true}), /did not answer/);
    assert.throws(() => lookup({failure: -600}), /Apple event error -600/);
});
