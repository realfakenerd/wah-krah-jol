"""Darwin cache-aware headroom contract v2. No command runs at import.

The labeled free-plus-inactive estimate follows psutil/Zabbix on macOS.
It is not a clean-page count or a guarantee of immediate allocation safety.
"""
from decimal import Decimal
import math
import re
import time

GIB = 1024 ** 3
SCHEMA = 'darwin-headroom-v2-cache-estimate'
FIELDS = ('Pages free', 'Pages speculative', 'Pages inactive',
          'File-backed pages', 'Pages purgeable')
PRESSURE = {1: 'normal', 2: 'warning', 4: 'critical'}


class HostContractError(RuntimeError):
    pass


def require(condition, reason):
    if not condition:
        raise HostContractError(reason)


def natural(value):
    return type(value) is int and value >= 0


def integer(text):
    require(re.fullmatch(r'[0-9]+\s*', text) is not None, 'malformed integer')
    return int(text)


def parse_vm_stat(text):
    headers = re.findall(r'^Mach Virtual Memory Statistics: \(page size of ([0-9]+) bytes\)\s*$', text, re.M)
    require(len(headers) == 1, 'missing/duplicate vm_stat header')
    page = int(headers[0])
    require(page in (4096, 16384), 'unsupported page size')
    pages = {}
    for label in FIELDS:
        values = re.findall(r'^' + re.escape(label) + r':\s*([0-9]+)\.\s*$', text, re.M)
        occurrences = re.findall(r'^' + re.escape(label) + r':', text, re.M)
        require(len(values) == len(occurrences) == 1, 'missing/malformed/duplicate ' + label)
        pages[label] = int(values[0])
    return page, pages


def parse_swap(text):
    # vm.swapusage is binary K/M/G. Decimal avoids binary floating point rounding.
    matches = re.findall(r'\bused\s*=\s*([0-9]+(?:\.[0-9]+)?)([KMG])(?=\s|$)', text)
    require(len(matches) == 1 and len(re.findall(r'\bused\s*=', text)) == 1,
            'missing/malformed/duplicate swap used')
    number, unit = matches[0]
    return int(Decimal(number) * 1024 ** ('KMG'.index(unit) + 1))


def parse_pressure(text):
    level = integer(text)
    require(level in PRESSURE, 'unknown pressure level')
    return level


def parse_query_percent(text):
    """Diagnostic only: never convert this percentage into a byte reserve."""
    values = re.findall(r'^System-wide memory free percentage:\s*([0-9]+)%\s*$', text, re.M)
    require(len(values) == 1 and 0 <= int(values[0]) <= 100, 'unknown pressure-query percentage')
    return int(values[0])


def snapshot(vm_text, physical_text, swap_text, pressure_before, pressure_after,
             started, finished):
    require(all(type(t) in (int, float) and math.isfinite(t) for t in (started, finished))
            and 0 <= finished - started <= 4, 'invalid/overlong host sample')
    physical = integer(physical_text)
    page, counts = parse_vm_stat(vm_text)
    require(physical > 0 and physical % page == 0, 'invalid physical memory size')
    require(all(v <= physical // page for v in counts.values()), 'VM counter exceeds physical memory')
    free = (counts['Pages free'] + counts['Pages speculative']) * page
    require(free <= physical, 'free/speculative exceeds physical memory')
    before, after = parse_pressure(pressure_before), parse_pressure(pressure_after)
    # Mach raw free already includes speculative pages; vm_stat displays them
    # separately. Inactive is a different queue. Do not add file-backed/purgeable
    # classifications: those can overlap the queue counts.
    estimate = free + counts['Pages inactive'] * page
    require(estimate <= physical, 'inconsistent free-plus-inactive estimate')
    return dict(schema=SCHEMA, sample_started=started, sample_finished=finished,
                physical_bytes=physical, page_bytes=page, vm_pages=counts,
                observed_free_bytes=counts['Pages free'] * page,
                observed_speculative_bytes=counts['Pages speculative'] * page,
                free_speculative_bytes=free,
                headroom_estimate_bytes=estimate,
                headroom_estimate_convention='psutil-zabbix-osx-free-plus-inactive-v1',
                headroom_estimate_is_guarantee=False,
                pressure_before=before, pressure_after=after,
                pressure=PRESSURE[max(before, after)], swap=parse_swap(swap_text))


def collect(command, clock=time.monotonic):
    """Collect one bracketed read-only host sample for the managed launcher."""
    started = clock()
    physical_and_pressure = command(['/usr/sbin/sysctl', '-n', 'hw.memsize',
                                    'kern.memorystatus_vm_pressure_level']).splitlines()
    require(len(physical_and_pressure) == 2, 'unknown physical/pressure response')
    vm_text = command(['/usr/bin/vm_stat'])
    swap_text = command(['/usr/sbin/sysctl', '-n', 'vm.swapusage'])
    after = command(['/usr/sbin/sysctl', '-n', 'kern.memorystatus_vm_pressure_level'])
    return snapshot(vm_text, physical_and_pressure[0], swap_text,
                    physical_and_pressure[1], after, started, clock())


def assess(host, initial_swap, profile, admission, now):
    """Return pass/stop/indeterminate. Only pass permits automatic continuation."""
    require(host.get('schema') == SCHEMA, 'host schema mismatch')
    require(profile in ('cpu', 'native') and type(admission) is bool, 'unknown reserve policy')
    require(type(now) in (int, float) and math.isfinite(now), 'invalid sample time')
    require(0 <= now - host['sample_finished'] <= 1, 'stale/future host sample')
    require(natural(initial_swap) and natural(host['swap']), 'invalid swap bytes')
    require(host['headroom_estimate_convention'] == 'psutil-zabbix-osx-free-plus-inactive-v1'
            and host['headroom_estimate_is_guarantee'] is False, 'unknown estimate convention')
    require(natural(host['headroom_estimate_bytes'])
            and host['headroom_estimate_bytes'] == host['free_speculative_bytes']
            + host['vm_pages']['Pages inactive'] * host['page_bytes']
            and host['free_speculative_bytes'] <= host['headroom_estimate_bytes'] <= host['physical_bytes'],
            'inconsistent headroom estimate')
    reserve = ({'cpu': (4, 2), 'native': (12, 8)}[profile][0 if admission else 1]) * GIB
    swap_cap = 256 * 1024 ** 2 if profile == 'cpu' else GIB
    reason = None
    state = 'stop'
    if host['pressure'] != 'normal':
        reason = 'elevated_pressure'
    elif host['swap'] - initial_swap >= swap_cap:
        reason = 'swap_growth_limit'
    elif host['headroom_estimate_bytes'] >= reserve:
        state, reason = 'pass', 'estimated_headroom_reserve_met'
    else:
        state, reason = 'stop', 'estimated_headroom_below_reserve'
    return dict(state=state, reason=reason, reserve_bytes=reserve,
                swap_growth_bytes=host['swap'] - initial_swap,
                swap_growth_abort_bytes=swap_cap, policy='cache-estimate-with-pressure-and-swap-v1',
                headroom_estimate_bytes=host['headroom_estimate_bytes'],
                observed_free_speculative_bytes=host['free_speculative_bytes'],
                estimate_is_guarantee=False)


def enforce(host, initial_swap, profile, admission=False, now=None):
    result = assess(host, initial_swap, profile, admission,
                    time.monotonic() if now is None else now)
    require(result['state'] == 'pass', 'host ' + result['state'] + ': ' + result['reason'])
    return result
