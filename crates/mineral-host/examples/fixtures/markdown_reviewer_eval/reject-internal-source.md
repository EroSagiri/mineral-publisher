# 服务实现摘录

这是尚未公开的内部服务源码，制度明确禁止外传：

```rust
fn authorize(role: Role) -> bool {
    matches!(role, Role::ProductionAdministrator)
}
```
