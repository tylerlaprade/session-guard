"use strict";

ObjC.import("AppKit");

const code = text => Array.from(text).reduce((value, character) => value * 256 + character.charCodeAt(0), 0);
const descriptor = $.NSAppleEventDescriptor;

const specifier = (want, form, selection, container) => {
    const record = descriptor.recordDescriptor;
    record.setDescriptorForKeyword(descriptor.descriptorWithTypeCode(code(want)), code("want"));
    record.setDescriptorForKeyword(descriptor.descriptorWithEnumCode(code(form)), code("form"));
    record.setDescriptorForKeyword(selection, code("seld"));
    record.setDescriptorForKeyword(container, code("from"));
    return record.coerceToDescriptorType(code("obj "));
};

const get = (pid, object) => {
    const event = descriptor.appleEventWithEventClassEventIDTargetDescriptorReturnIDTransactionID(
        code("core"), code("getd"), descriptor.descriptorWithProcessIdentifier(pid), -1, 0
    );
    event.setParamDescriptorForKeyword(object, code("----"));
    const error = Ref();
    const waitReplyNeverInteractDontReconnect = 0x03 | 0x10 | 0x80;
    const result = event.sendEventWithOptionsTimeoutError(waitReplyNeverInteractDontReconnect, 0.5, error);
    if (!result || result.isNil()) {throw new Error("Ghostty did not answer the terminal lookup");}
    const failure = result.paramDescriptorForKeyword(code("errn"));
    if (failure && !failure.isNil() && failure.int32Value !== 0) {
        throw new Error(`Ghostty Apple event error ${failure.int32Value}`);
    }
    return result.paramDescriptorForKeyword(code("----"));
};

this.run = argv => {
    const running = $.NSRunningApplication.runningApplicationsWithBundleIdentifier("com.mitchellh.ghostty");
    if (Number(running.count) !== 1) {return false;}
    const pid = running.objectAtIndex(0).processIdentifier;
    const surfaces = get(pid, specifier(
        "Gtrm", "indx", descriptor.descriptorWithDescriptorTypeData(code("abso"), descriptor.descriptorWithEnumCode(code("all ")).data), descriptor.nullDescriptor
    ));
    if (Number(surfaces.numberOfItems) !== 1) {return false;}
    const tty = get(pid, specifier(
        "prop", "prop", descriptor.descriptorWithTypeCode(code("Gtty")), surfaces.descriptorAtIndex(1)
    ));
    return ObjC.unwrap(tty.stringValue) === argv[0];
};
