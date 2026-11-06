// Freestanding C++20 workload: templates, RAII, virtual dispatch, iterators,
// insertion sort, bounded shortest paths, function pointers, and data tables.
using u32 = __UINT32_TYPE__;
using i32 = __INT32_TYPE__;
using usize = __SIZE_TYPE__;
#define KEEP __attribute__((noinline))
#define EXPORT(name) extern "C" __attribute__((export_name(name)))
extern "C" void* memset(void* p, int v, usize n) {
    auto* b = static_cast<unsigned char*>(p);
    for (usize i = 0; i < n; ++i) b[i] = static_cast<unsigned char>(v);
    return p;
}
extern "C" void* memcpy(void* d, const void* s, usize n) {
    auto* dst = static_cast<unsigned char*>(d);
    const auto* src = static_cast<const unsigned char*>(s);
    for (usize i = 0; i < n; ++i) dst[i] = src[i];
    return d;
}
namespace audit_case {
template<class T, u32 N> struct Buffer {
    T values[N];
    T* begin() { return values; }
    T* end() { return values + N; }
    T& operator[](u32 i) { return values[i]; }
};
struct Item {
    u32 quantity;
    u32 price;
    u32 category;
};
struct Record {
    u32 sku;
    u32 price; // layout insertion point
    u32 quantity;
};
struct Counter {
    u32& total;
    explicit Counter(u32& v): total(v) { total += 1; }
    ~Counter() { total += 3; } // RAII change
};
struct Rule {
    virtual u32 charge(const Item& item) const { return item.price; }
};
struct Retail final : Rule {
    KEEP u32 charge(const Item& item) const override {
        return item.quantity * item.price + item.category * 3;
    }
};
struct Wholesale final : Rule {
    KEEP u32 charge(const Item& item) const override {
        return item.quantity * item.price - item.quantity / 4;
    }
};
KEEP u32 dispatch(const Rule& rule, const Item& item) { return rule.charge(item); }
KEEP u32 domestic_fee(u32 x) { return x * 7 + 3; } // collision target
KEEP u32 foreign_fee(u32 x) { return x * 11 + 3; }
KEEP u32 discount(u32 x) { return x - x / 7; }
KEEP u32 surcharge(u32 x) { return x + x / 5; }
KEEP u32 apply_fee(u32 x, u32 mode) {
    using Fee = u32 (*)(u32);
    const Fee fees[] = {domestic_fee, foreign_fee};
    return fees[mode & 1](x);
}
template<u32 Salt> KEEP u32 mix(u32 value) {
    value ^= value >> 13;
    value *= Salt;
    return value ^ (value >> 9);
}
KEEP u32 classify(u32 value) {
    if (value > 127) return 3; // boundary change
    if (value > 63) return 2;
    return 1;
}
KEEP u32 record_price(const Record& r) { return r.price; }
KEEP u32 record_quantity(const Record& r) { return r.quantity; }
template<class Iterator, class Compare> KEEP void insertion_sort(Iterator first, Iterator last, Compare compare) {
    for (auto it = first + 1; it != last; ++it) {
        auto value = *it;
        auto pos = it;
        while (pos != first && compare(value, pos[-1])) {
            *pos = pos[-1];
            --pos;
        }
        *pos = value;
    }
}
struct Priority {
    bool operator()(const Item& a, const Item& b) const {
        return a.price < b.price; // ordering change
    }
};
KEEP u32 route(u32 seed) {
    Buffer<u32, 12> distance;
    Buffer<u32, 12> done;
    for (u32 i = 0; i < 12; ++i) { distance[i] = 0x3fffffff; done[i] = 0; }
    distance[0] = 0;
    for (u32 pass = 0; pass < 12; ++pass) {
        u32 best = 12, cost = 0x3fffffff;
        for (u32 i = 0; i < 12; ++i)
            if (!done[i] && distance[i] < cost) { best = i; cost = distance[i]; }
        if (best == 12) break;
        done[best] = 1;
        for (u32 j = 0; j < 12; ++j) {
            u32 edge = ((best * 17 + j * 13 + seed) % 23) + 1;
            if (((best + j + seed) & 3) == 0) continue;
            if (cost + edge < distance[j]) distance[j] = cost + edge;
        }
    }
    return distance[11];
}
static u32 table[] = {13, 17, 19, 23, 29, 31, 37, 41}; // table change
KEEP u32 lookup(u32 x) { return table[x & 7]; }
KEEP u32 evaluate(u32 seed, u32 mode) {
    Buffer<Item, 24> items;
    Retail retail;
    Wholesale wholesale;
    const Rule& rule = (mode & 1) ? static_cast<const Rule&>(wholesale) : retail;
    u32 events = 0, sum = 0;
    for (u32 i = 0; i < 24; ++i) {
        Counter scope(events);
        items[i] = {(seed + i) % 9 + 1, mix<2654435761u>(seed + i) % 251, i % 3};
    }
    insertion_sort(items.begin(), items.end(), Priority{});
    for (u32 i = 0; i < 24; ++i) {
        sum += dispatch(rule, items[i]) * (i + 1);
        sum ^= mix<2246822519u>(items[i].price);
    }
    Record record{seed, sum, events};
    sum += record_price(record) + record_quantity(record);
    sum += route(seed) + lookup(seed) + classify(seed);
    sum = discount(sum); // direct target change
    return apply_fee(sum, mode) ^ events;
}
KEEP u32 eliminated(u32 x) { return x * 97; } // dead source change
}
EXPORT("evaluate") u32 evaluate_export(u32 seed, u32 mode) { return audit_case::evaluate(seed, mode); }
EXPORT("fees") u32 fees_export(u32 x, u32 mode) { return audit_case::apply_fee(x, mode); }
EXPORT("boundary") u32 boundary_export(u32 x) { return audit_case::classify(x); }
EXPORT("table_value") u32 table_export(u32 x) { return audit_case::lookup(x); }
// Keep both potential direct call targets reachable for the retargeting case.
EXPORT("alternate") u32 alternate_export(u32 x) { return audit_case::surcharge(x); }
static u32 sequence = 0;
EXPORT("atomic_step") u32 atomic_export(u32 delta) {
    u32 step = delta + 1; // atomic increment change
    return __atomic_fetch_add(&sequence, step, __ATOMIC_SEQ_CST);
}
