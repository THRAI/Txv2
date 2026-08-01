# 暂缓在决赛根 ext4 启用异步 Page-I/O

## 当前决定

决赛根文件系统继续采用 `final-smp` 已验证的同步 ext4 backing 路径。main 的异步
Page-I/O、planner 和 ordered-journal 代码继续保留，但本次合并不在根挂载上启用。

同步挂载必须满足一个边界：没有绑定 File-I/O service runtime 时，VFS 的 `fsync`
直接调用 `FsPageBacking::fsync_file`，close 也不能把脏页转成无人消费的 Writeback
请求。这样保留 final-smp 的行为，同时避免请求进入无消费者队列后永久等待。

## 暂缓原因

main 的异步实现目前不能只通过替换 mount 函数启用，完整链路还要求：

1. 根挂载使用带 planner 和 ordered journal 的 ext4 mount。
2. `MountPayload` 必须保存同一个 backend planner，不能使用无 planner 构造函数。
3. 每个文件 PageContainer 必须通过 `Ext4FileIoRuntimeBinder` 绑定块设备和 wake source。
4. reactor 初始化后必须提交对应的 File-I/O service task。
5. runtime 注册表和 service task 不能永久强引用每个 PageContainer。当前“一文件一永久
   task”的形状会在 BuildStorm 中累积任务和页缓存，需要先改成可退出的弱引用 runtime，
   或改成每挂载/每设备共享的有界 worker。

只完成前四项虽然能让异步请求被消费，但会带来明显的生命周期和扩展性风险，因此不作为
本次正确性合并的一部分。

## 后续启用条件

- 为异步 runtime 实现有界、可回收的生命周期。
- 增加“planner、binder、service task 三者一致”的挂载级测试。
- 验证同步 fallback、异步 fsync、close writeback、进程退出和卸载路径。
- 用 BuildStorm 比较任务数量、PageContainer 存活量、吞吐和内存占用，再切换根挂载默认值。

