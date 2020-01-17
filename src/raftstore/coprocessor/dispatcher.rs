// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use engine::rocks::DB;
use engine::CfName;
use kvproto::metapb::Region;
use kvproto::pdpb::CheckPolicy;
use kvproto::raft_cmdpb::{RaftCmdRequest, RaftCmdResponse};
use std::mem;

use crate::raftstore::store::CasualRouter;

use super::*;

struct Entry<T> {
    priority: u32,
    observer: T,
}

// TODO: change it to Send + Clone.
pub type BoxAdminObserver = Box<dyn AdminObserver + Send + Sync>;
pub type BoxQueryObserver = Box<dyn QueryObserver + Send + Sync>;
pub type BoxApplySnapshotObserver = Box<dyn ApplySnapshotObserver + Send + Sync>;
pub type BoxSplitCheckObserver = Box<dyn SplitCheckObserver + Send + Sync>;
pub type BoxRoleObserver = Box<dyn RoleObserver + Send + Sync>;
pub type BoxRegionChangeObserver = Box<dyn RegionChangeObserver + Send + Sync>;
pub type BoxCmdObserver = Box<dyn CmdObserver + Send + Sync>;

/// Registry contains all registered coprocessors.
#[derive(Default)]
pub struct Registry {
    admin_observers: Vec<Entry<BoxAdminObserver>>,
    query_observers: Vec<Entry<BoxQueryObserver>>,
    apply_snapshot_observers: Vec<Entry<BoxApplySnapshotObserver>>,
    split_check_observers: Vec<Entry<BoxSplitCheckObserver>>,
    role_observers: Vec<Entry<BoxRoleObserver>>,
    region_change_observers: Vec<Entry<BoxRegionChangeObserver>>,
    cmd_observers: Vec<Entry<BoxCmdObserver>>,
    // TODO: add endpoint
}

macro_rules! push {
    ($p:expr, $t:ident, $vec:expr) => {
        $t.start();
        let e = Entry {
            priority: $p,
            observer: $t,
        };
        let vec = &mut $vec;
        vec.push(e);
        vec.sort_by(|l, r| l.priority.cmp(&r.priority));
    };
}

impl Registry {
    pub fn register_admin_observer(&mut self, priority: u32, ao: BoxAdminObserver) {
        push!(priority, ao, self.admin_observers);
    }

    pub fn register_query_observer(&mut self, priority: u32, qo: BoxQueryObserver) {
        push!(priority, qo, self.query_observers);
    }

    pub fn register_apply_snapshot_observer(
        &mut self,
        priority: u32,
        aso: BoxApplySnapshotObserver,
    ) {
        push!(priority, aso, self.apply_snapshot_observers);
    }

    pub fn register_split_check_observer(&mut self, priority: u32, sco: BoxSplitCheckObserver) {
        push!(priority, sco, self.split_check_observers);
    }

    pub fn register_role_observer(&mut self, priority: u32, ro: BoxRoleObserver) {
        push!(priority, ro, self.role_observers);
    }

    pub fn register_region_change_observer(&mut self, priority: u32, rlo: BoxRegionChangeObserver) {
        push!(priority, rlo, self.region_change_observers);
    }

    pub fn register_cmd_observer(&mut self, priority: u32, rlo: BoxCmdObserver) {
        push!(priority, rlo, self.cmd_observers);
    }
}

/// A macro that loops over all observers and returns early when error is found or
/// bypass is set. `try_loop_ob` is expected to be used for hook that returns a `Result`.
macro_rules! try_loop_ob {
    ($r:expr, $obs:expr, $hook:ident, $($args:tt)*) => {
        loop_ob!(_imp _res, $r, $obs, $hook, $($args)*)
    };
}

/// A macro that loops over all observers and returns early when bypass is set.
///
/// Using a macro so we don't need to write tests for every observers.
macro_rules! loop_ob {
    // Execute a hook, return early if error is found.
    (_exec _res, $o:expr, $hook:ident, $ctx:expr, $($args:tt)*) => {
        $o.$hook($ctx, $($args)*)?
    };
    // Execute a hook.
    (_exec _tup, $o:expr, $hook:ident, $ctx:expr, $($args:tt)*) => {
        $o.$hook($ctx, $($args)*)
    };
    // When the try loop finishes successfully, the value to be returned.
    (_done _res) => {
        Ok(())
    };
    // When the loop finishes successfully, the value to be returned.
    (_done _tup) => {{}};
    // Actual implementation of the for loop.
    (_imp $res_type:tt, $r:expr, $obs:expr, $hook:ident, $($args:tt)*) => {{
        let mut ctx = ObserverContext::new($r);
        for o in $obs {
            loop_ob!(_exec $res_type, o.observer, $hook, &mut ctx, $($args)*);
            if ctx.bypass {
                break;
            }
        }
        loop_ob!(_done $res_type)
    }};
    // Loop over all observers and return early when bypass is set.
    // This macro is expected to be used for hook that returns `()`.
    ($r:expr, $obs:expr, $hook:ident, $($args:tt)*) => {
        loop_ob!(_imp _tup, $r, $obs, $hook, $($args)*)
    };
}

/// Admin and invoke all coprocessors.
#[derive(Default)]
pub struct CoprocessorHost {
    pub registry: Registry,
}

impl CoprocessorHost {
    pub fn new<C: CasualRouter + Clone + Send + 'static>(ch: C) -> CoprocessorHost {
        let mut registry = Registry::default();
        registry.register_split_check_observer(200, Box::new(SizeCheckObserver::new(ch.clone())));
        registry.register_split_check_observer(200, Box::new(KeysCheckObserver::new(ch)));
        // TableCheckObserver has higher priority than SizeCheckObserver.
        registry.register_split_check_observer(100, Box::new(HalfCheckObserver));
        registry.register_split_check_observer(400, Box::new(TableCheckObserver::default()));
        CoprocessorHost { registry }
    }

    /// Call all prepose hooks until bypass is set to true.
    pub fn pre_propose(&self, region: &Region, req: &mut RaftCmdRequest) -> Result<()> {
        if !req.has_admin_request() {
            let query = req.mut_requests();
            let mut vec_query = mem::take(query).into();
            let result = try_loop_ob!(
                region,
                &self.registry.query_observers,
                pre_propose_query,
                &mut vec_query,
            );
            *query = vec_query.into();
            result
        } else {
            let admin = req.mut_admin_request();
            try_loop_ob!(
                region,
                &self.registry.admin_observers,
                pre_propose_admin,
                admin
            )
        }
    }

    /// Call all pre apply hook until bypass is set to true.
    pub fn pre_apply(&self, region: &Region, index: u64, req: &RaftCmdRequest) {
        if !req.has_admin_request() {
            let query = req.get_requests();
            loop_ob!(
                region,
                &self.registry.query_observers,
                pre_apply_query,
                query,
            );
        } else {
            let admin = req.get_admin_request();
            loop_ob!(
                region,
                &self.registry.admin_observers,
                pre_apply_admin,
                index,
                admin,
            );
        }
    }

    pub fn post_apply(&self, region: &Region, index: u64, resp: &mut RaftCmdResponse) {
        let header = resp.header.get_ref();
        if !resp.has_admin_response() {
            let mut vec_query = mem::take(&mut resp.responses).into();
            loop_ob!(
                region,
                &self.registry.query_observers,
                post_apply_query,
                header,
                &mut vec_query,
            );
            resp.responses = vec_query.into();
        } else {
            let admin = resp.admin_response.get_mut_ref();
            loop_ob!(
                region,
                &self.registry.admin_observers,
                post_apply_admin,
                index,
                header,
                admin
            );
        }
    }

    pub fn pre_apply_plain_kvs_from_snapshot(
        &self,
        region: &Region,
        cf: CfName,
        kv_pairs: &[(Vec<u8>, Vec<u8>)],
    ) {
        loop_ob!(
            region,
            &self.registry.apply_snapshot_observers,
            pre_apply_plain_kvs,
            cf,
            kv_pairs
        );
    }

    pub fn pre_apply_sst_from_snapshot(&self, region: &Region, cf: CfName, path: &str) {
        loop_ob!(
            region,
            &self.registry.apply_snapshot_observers,
            pre_apply_sst,
            cf,
            path
        );
    }

    pub fn new_split_checker_host<'a>(
        &self,
        cfg: &'a Config,
        region: &Region,
        engine: &DB,
        auto_split: bool,
        policy: CheckPolicy,
    ) -> SplitCheckerHost<'a> {
        let mut host = SplitCheckerHost::new(auto_split, cfg);
        loop_ob!(
            region,
            &self.registry.split_check_observers,
            add_checker,
            &mut host,
            engine,
            policy
        );
        host
    }

    pub fn on_role_change(&self, region: &Region, role: StateRole) {
        loop_ob!(region, &self.registry.role_observers, on_role_change, role);
    }

    pub fn on_region_changed(&self, region: &Region, event: RegionChangeEvent, role: StateRole) {
        loop_ob!(
            region,
            &self.registry.region_change_observers,
            on_region_changed,
            event,
            role
        );
    }

    pub fn on_cmd_executed(&self, batch: &[CmdBatch]) {
        for cmd_ob in &self.registry.cmd_observers {
            cmd_ob.observer.on_batch_executed(batch)
        }
    }

    pub fn shutdown(&self) {
        for entry in &self.registry.admin_observers {
            entry.observer.stop();
        }
        for entry in &self.registry.query_observers {
            entry.observer.stop();
        }
        for entry in &self.registry.split_check_observers {
            entry.observer.stop();
        }
        for entry in &self.registry.cmd_observers {
            entry.observer.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::raftstore::coprocessor::*;
    use std::sync::atomic::*;
    use std::sync::*;

    use kvproto::metapb::Region;
    use kvproto::raft_cmdpb::{
        AdminRequest, AdminResponse, RaftCmdRequest, RaftCmdResponse, Request, Response,
    };

    #[derive(Clone, Default)]
    struct TestCoprocessor {
        bypass: Arc<AtomicBool>,
        called: Arc<AtomicUsize>,
        return_err: Arc<AtomicBool>,
    }

    impl Coprocessor for TestCoprocessor {}

    impl AdminObserver for TestCoprocessor {
        fn pre_propose_admin(
            &self,
            ctx: &mut ObserverContext<'_>,
            _: &mut AdminRequest,
        ) -> Result<()> {
            self.called.fetch_add(1, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
            if self.return_err.load(Ordering::SeqCst) {
                return Err(box_err!("error"));
            }
            Ok(())
        }

        fn pre_apply_admin(&self, ctx: &mut ObserverContext<'_>, _: u64, _: &AdminRequest) {
            self.called.fetch_add(2, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }

        fn post_apply_admin(
            &self,
            ctx: &mut ObserverContext<'_>,
            _: u64,
            _: &RaftResponseHeader,
            _: &mut AdminResponse,
        ) {
            self.called.fetch_add(3, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }
    }

    impl QueryObserver for TestCoprocessor {
        fn pre_propose_query(
            &self,
            ctx: &mut ObserverContext<'_>,
            _: &mut Vec<Request>,
        ) -> Result<()> {
            self.called.fetch_add(4, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
            if self.return_err.load(Ordering::SeqCst) {
                return Err(box_err!("error"));
            }
            Ok(())
        }

        fn pre_apply_query(&self, ctx: &mut ObserverContext<'_>, _: &[Request]) {
            self.called.fetch_add(5, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }

        fn post_apply_query(
            &self,
            ctx: &mut ObserverContext<'_>,
            _: &RaftResponseHeader,
            _: &mut Vec<Response>,
        ) {
            self.called.fetch_add(6, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }
    }

    impl RoleObserver for TestCoprocessor {
        fn on_role_change(&self, ctx: &mut ObserverContext<'_>, _: StateRole) {
            self.called.fetch_add(7, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }
    }

    impl RegionChangeObserver for TestCoprocessor {
        fn on_region_changed(
            &self,
            ctx: &mut ObserverContext<'_>,
            _: RegionChangeEvent,
            _: StateRole,
        ) {
            self.called.fetch_add(8, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }
    }

    impl ApplySnapshotObserver for TestCoprocessor {
        fn pre_apply_plain_kvs(
            &self,
            ctx: &mut ObserverContext<'_>,
            _: CfName,
            _: &[(Vec<u8>, Vec<u8>)],
        ) {
            self.called.fetch_add(9, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }

        fn pre_apply_sst(&self, ctx: &mut ObserverContext<'_>, _: CfName, _: &str) {
            self.called.fetch_add(10, Ordering::SeqCst);
            ctx.bypass = self.bypass.load(Ordering::SeqCst);
        }
    }

    impl CmdObserver for TestCoprocessor {
        fn on_batch_executed(&self, _: &[CmdBatch]) {
            self.called.fetch_add(9, Ordering::SeqCst);
        }
    }

    macro_rules! assert_all {
        ($target:expr, $expect:expr) => {{
            for (c, e) in ($target).iter().zip($expect) {
                assert_eq!(c.load(Ordering::SeqCst), *e);
            }
        }};
    }

    macro_rules! set_all {
        ($target:expr, $v:expr) => {{
            for v in $target {
                v.store($v, Ordering::SeqCst);
            }
        }};
    }

    #[test]
    fn test_trigger_right_hook() {
        let mut host = CoprocessorHost::default();
        let ob = TestCoprocessor::default();
        host.registry
            .register_admin_observer(1, Box::new(ob.clone()));
        host.registry
            .register_query_observer(1, Box::new(ob.clone()));
        host.registry
            .register_apply_snapshot_observer(1, Box::new(ob.clone()));
        host.registry
            .register_role_observer(1, Box::new(ob.clone()));
        host.registry
            .register_region_change_observer(1, Box::new(ob.clone()));
        host.registry.register_cmd_observer(1, Box::new(ob.clone()));
        let region = Region::default();
        let mut admin_req = RaftCmdRequest::default();
        admin_req.set_admin_request(AdminRequest::default());
        host.pre_propose(&region, &mut admin_req).unwrap();
        assert_all!(&[&ob.called], &[1]);
        host.pre_apply(&region, 0, &admin_req);
        assert_all!(&[&ob.called], &[3]);
        let mut admin_resp = RaftCmdResponse::default();
        admin_resp.set_admin_response(AdminResponse::default());
        admin_resp.header = Some(RaftResponseHeader::default()).into();
        host.post_apply(&region, 0, &mut admin_resp);
        assert_all!(&[&ob.called], &[6]);

        let mut query_req = RaftCmdRequest::default();
        query_req.set_requests(vec![Request::default()].into());
        host.pre_propose(&region, &mut query_req).unwrap();
        assert_all!(&[&ob.called], &[10]);
        host.pre_apply(&region, 0, &query_req);
        assert_all!(&[&ob.called], &[15]);
        let mut query_resp = admin_resp.clone();
        query_resp.clear_admin_response();
        host.post_apply(&region, 0, &mut query_resp);
        assert_all!(&[&ob.called], &[21]);

        host.on_role_change(&region, StateRole::Leader);
        assert_all!(&[&ob.called], &[28]);

        host.on_region_changed(&region, RegionChangeEvent::Create, StateRole::Follower);
        assert_all!(&[&ob.called], &[36]);

        host.on_cmd_executed(&[]);
        assert_all!(&[&ob.called], &[45]);

        host.pre_apply_plain_kvs_from_snapshot(&region, "default", &[]);
        assert_all!(&[&ob.called], &[45]);
        host.pre_apply_sst_from_snapshot(&region, "default", "");
        assert_all!(&[&ob.called], &[55]);
    }

    #[test]
    fn test_order() {
        let mut host = CoprocessorHost::default();

        let ob1 = TestCoprocessor::default();
        host.registry
            .register_admin_observer(3, Box::new(ob1.clone()));
        host.registry
            .register_query_observer(3, Box::new(ob1.clone()));
        let ob2 = TestCoprocessor::default();
        host.registry
            .register_admin_observer(2, Box::new(ob2.clone()));
        host.registry
            .register_query_observer(2, Box::new(ob2.clone()));

        let region = Region::default();
        let mut admin_req = RaftCmdRequest::default();
        admin_req.set_admin_request(AdminRequest::default());
        let mut admin_resp = RaftCmdResponse::default();
        admin_resp.set_admin_response(AdminResponse::default());
        let query_req = RaftCmdRequest::default();
        let query_resp = RaftCmdResponse::default();

        let cases = vec![(0, admin_req, admin_resp), (3, query_req, query_resp)];

        for (base_score, mut req, mut resp) in cases {
            set_all!(&[&ob1.return_err, &ob2.return_err], false);
            set_all!(&[&ob1.called, &ob2.called], 0);
            set_all!(&[&ob1.bypass, &ob2.bypass], true);

            host.pre_propose(&region, &mut req).unwrap();

            // less means more.
            assert_all!(&[&ob1.called, &ob2.called], &[0, base_score + 1]);

            host.pre_apply(&region, 0, &req);
            assert_all!(&[&ob1.called, &ob2.called], &[0, base_score * 2 + 3]);

            host.post_apply(&region, 0, &mut resp);
            assert_all!(&[&ob1.called, &ob2.called], &[0, base_score * 3 + 6]);

            set_all!(&[&ob2.bypass], false);
            set_all!(&[&ob2.called], 0);

            host.pre_propose(&region, &mut req).unwrap();

            assert_all!(
                &[&ob1.called, &ob2.called],
                &[base_score + 1, base_score + 1]
            );

            set_all!(&[&ob1.called, &ob2.called], 0);

            // when return error, following coprocessor should not be run.
            set_all!(&[&ob2.return_err], true);
            host.pre_propose(&region, &mut req).unwrap_err();
            assert_all!(&[&ob1.called, &ob2.called], &[0, base_score + 1]);
        }
    }
}
