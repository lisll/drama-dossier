-- 初始化时多建一个媒体库（M7-7）。
--
-- 两个服务的表结构互不兼容，必须各一个库：
-- commerce 的 users.id 是 uuid，媒体侧的 projects.owner_id 是 text，
-- 混在一个库里建表会直接撞在外键类型上。
--
-- /docker-entrypoint-initdb.d 里的脚本只在**数据卷为空**时执行一次；
-- 已有数据的库不会重跑，这是官方的行为，不需要额外判断。

CREATE DATABASE drama_media;
