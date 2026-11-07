"""Strict decoder for wasm-git.command binary-v1. All integers are big-endian.

The frame and every length are checked before accessing a value. This decoder
does not trust a sender's allocation sizes, nesting, UTF-8 or numeric tags.
"""
import math
import struct

MAGIC = b'WGTCLI\x00\x01'
MAX_FRAME = 32 * 1024 * 1024


def response(record, exit_code):
    """Validate the envelope separately from the wire value decoder."""
    valid = (isinstance(record, dict) and record.get('protocol') == 'wasm-git.command'
             and type(record.get('version')) is int and record['version'] == 1
             and type(record.get('exit_code')) is int and record['exit_code'] == exit_code
             and type(record.get('ok')) is bool and record['ok'] == (exit_code == 0)
             and isinstance(record.get('command'), str))
    if not valid:
        raise ValueError('Invalid command response envelope or exit status.')
    result, error = record.get('result'), record.get('error')
    if not isinstance(result, dict) or result.get('kind') not in ('data', 'text', 'empty'):
        raise ValueError('Unknown command response result kind.')
    if result['kind'] == 'data' and not isinstance(result.get('value'), (dict, list)):
        raise ValueError('Invalid structured command response.')
    if result['kind'] == 'text' and not isinstance(result.get('text'), str):
        raise ValueError('Invalid text command response.')
    if error is not None and (not isinstance(error, dict) or not isinstance(error.get('code'), str)
                              or not isinstance(error.get('message'), str)):
        raise ValueError('Invalid command error response.')
    return record


def decode(data):
    if len(data) < 16 or len(data) > MAX_FRAME or data[:8] != MAGIC:
        raise ValueError('Invalid command response header or size.')
    if int.from_bytes(data[8:16], 'big') != len(data) - 16:
        raise ValueError('Invalid command response frame length.')
    position = 16
    nodes = 0

    def take(size):
        nonlocal position
        if size < 0 or size > len(data) - position:
            raise ValueError('Truncated command response.')
        start = position
        position += size
        return data[start:position]

    def length():
        size = int.from_bytes(take(8), 'big')
        if size > len(data) - position:
            raise ValueError('Invalid command response field length.')
        return size

    def string():
        return take(length()).decode('utf-8', 'strict')

    def value(depth=0):
        nonlocal nodes
        nodes += 1
        if depth > 128 or nodes > 1_000_000:
            raise ValueError('Command response exceeds decoder complexity limit.')
        tag = take(1)[0]
        if tag == 0:
            return None
        if tag in (1, 2):
            return tag == 2
        if tag in (3, 4):
            result = int.from_bytes(take(8), 'big', signed=tag == 3)
            if tag == 3 and result >= 0:
                raise ValueError('Noncanonical signed integer.')
            return result
        if tag == 5:
            number = struct.unpack('>d', take(8))[0]
            if not math.isfinite(number):
                raise ValueError('Nonfinite command response number.')
            return number
        if tag == 6:
            return string()
        if tag == 7:
            count = length()
            return [value(depth + 1) for _ in range(count)]
        if tag == 8:
            count = length()
            if count > (len(data) - position) // 9:
                raise ValueError('Invalid command response map length.')
            result, previous = {}, None
            for _ in range(count):
                key = string()
                if previous is not None and key <= previous:
                    raise ValueError('Noncanonical command response map keys.')
                previous = key
                result[key] = value(depth + 1)
            return result
        raise ValueError('Unknown command response value tag.')

    result = value()
    if position != len(data):
        raise ValueError('Trailing command response bytes.')
    return result
