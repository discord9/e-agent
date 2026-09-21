# 宿主配置问题报告：render 组成员身份未进入登录会话，导致 /dev/kfd 无法打开

**面向**：负责这台机器初始化的 setup agent
**报告人**：e-agent 主 agent（在排查沙盒 GPU 直通时发现）
**机器**：Ubuntu 24.04.4 LTS，AMD GPU（amdgpu 驱动），用户 discord9 (uid 1000)
**日期**：2026-09-20
**严重级别**：阻塞 —— 所有 AMD GPU 计算工作负载当前无法运行

---

## 一句话结论

用户已正确加入 `render` 组（`/etc/group` 确认），但图形登录会话**从未把该组装入进程**（内核 `/proc/self/status` 佐证），导致 `open("/dev/kfd")` 返回 EACCES，ROCm 栈全部初始化失败。**不是 e-agent 代码问题，是宿主登录/PAM 配置缺陷。**

## 实测证据

```
$ ls -l /dev/kfd
crw-rw---- 1 root render 234, 0  9月 20 11:10 /dev/kfd     # 属 root:render (gid 992)

$ getent group render
render:x:992:discord9                                       # 用户确实在组里

$ id
uid=1000(discord9) gid=1000(discord9) 组=...,4(adm),...,114(lpadmin)
                                                              # ↑ 没有 render

$ cat /proc/self/status | grep -i groups
Groups: 4 24 27 30 46 100 114 1000                            # ↑ 内核层面也没有 992

$ exec 3</dev/kfd
bash: /dev/kfd: 权限不够                                      # EACCES

$ rocminfo
ROCk module is loaded
Unable to open /dev/kfd read-write: Permission denied
discord9 is member of render group                            # ROCm 都确认组配置是对的
```

**已排除的因素**：注销并重新登录图形会话后问题依旧（`id` 仍无 render）。作为对照，同机的 `/dev/dri/renderD128/129` 可以正常打开——说明两个设备节点的组/ACL 规则不一致，进一步印证是会话组缺失而非设备故障。

## 根因

Linux 辅助组成员身份只在**登录那一刻**由 PAM 从 `/etc/group` 读取并附加到会话。本机：

- `/etc/pam.d/login` 有 `auth optional pam_group.so`——这只覆盖 **text console** 登录。
- 图形登录（GDM/SDDM）+ systemd --user session 走**另一条 PAM stack**，没有把 `/etc/group` 的 render 组装进会话。

这是已知的 systemd/GDM 行为，非个例：
- https://github.com/systemd/systemd/issues/15659 （user service 不带辅助组）
- https://github.com/systemd/systemd/issues/11198 （DBus 发起的 systemd --user 会话组不全）

## 影响

- 所有依赖 `/dev/kfd` 的 AMD GPU 计算工作负载（rocminfo、PyTorch/ROCm HIP、amdsmi 等）当前在这台机器上**全部无法运行**。
- 阻塞 e-agent 沙盒 GPU 直通功能的端到端验证（该功能代码本身已验证正确，只差宿主这最后一道权限）。

## 立即可用的绕过方案（按 uid 授权，绕过组成员机制）

```sh
# 立即生效，无需重登
sudo setfacl -m u:discord9:rw /dev/kfd

# 验证
exec 3</dev/kfd && echo HOST-KFD-OPEN-OK
rocminfo | head -3
```

持久化（kfd 节点每次启动由 udev 重建，ACL 会丢，需固化）：

```sh
sudo tee /etc/udev/rules.d/70-kfd-acl.rules <<'EOF'
# Grant discord9 direct rw access to the AMD compute node, bypassing the
# render-group login-session issue.
KERNEL=="kfd", SUBSYSTEM=="kfd", ACTION=="add", RUN+="/usr/bin/setfacl -m u:discord9:rw /dev/kfd"
EOF
sudo udevadm control --reload-rules
```

## 请 setup agent 处理的根治项（二选一）

1. **修复图形登录的组集成**：让 render 组（及任何 `/etc/group` 里给用户的辅助组）正常进入 GDM/SDDM + systemd --user 会话。排查方向：`/etc/pam.d/gdm-password`、`/etc/pam.d/sddm`、`/etc/security/group.conf`，以及 `pam_systemd.so` 是否在某处重置了组。
2. **接受 ACL 方案为标准配置**：把上面的 udev 规则纳入机器初始化脚本，明确"本机 GPU 计算节点按 uid 授权，不依赖登录组"。

## 建议的验收标准

修复完成后，**新开一个登录会话**应满足：

```sh
id | grep render                       # 输出包含 render
cat /proc/self/status | grep Groups   # 包含 992
exec 3</dev/kfd && echo OK            # 无需 sudo / setfacl 即可打开
rocminfo | grep -i "gfx\|agent"       # 能枚举出 GPU
```
