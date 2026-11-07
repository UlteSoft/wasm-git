__attribute__((objc_root_class))
@interface AuditObject
- (unsigned)calculate:(unsigned)x;
@end
@implementation AuditObject
- (unsigned)calculate:(unsigned)x {
    return x * 7 + 11; // controlled edit: 11 -> 12
}
@end
// Exercise Objective-C method debug symbols without depending on a host runtime.
__attribute__((export_name("entry"))) unsigned entry(unsigned x) {
    return x * 7 + 11;
}
