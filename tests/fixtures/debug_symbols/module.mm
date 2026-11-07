namespace audit_demo {
inline unsigned scaled(unsigned x) { return x * 7; }
}
__attribute__((objc_root_class))
@interface AuditObject
- (unsigned)calculate:(unsigned)x;
@end
@implementation AuditObject
- (unsigned)calculate:(unsigned)x {
    return audit_demo::scaled(x) + 11; // controlled edit: 11 -> 12
}
@end
extern "C" __attribute__((export_name("entry"))) unsigned entry(unsigned x) {
    return audit_demo::scaled(x) + 11;
}
