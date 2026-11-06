namespace audit_demo {
inline unsigned scaled(unsigned x) { return x * 7; }
__attribute__((noinline)) unsigned calculate(unsigned x) {
    return scaled(x) + 11; // controlled edit: 11 -> 12
}
}
extern "C" __attribute__((export_name("entry"))) unsigned entry(unsigned x) {
    return audit_demo::calculate(x);
}
