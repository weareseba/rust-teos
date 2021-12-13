//! Logic related to the Gatekeeper, the component in charge of managing access to the tower resources.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use std::sync::{Arc, Mutex};

use lightning::chain;
use lightning_block_sync::poll::ValidatedBlockHeader;

use teos_common::constants::{ENCRYPTED_BLOB_MAX_SIZE, OUTDATED_USERS_CACHE_SIZE_BLOCKS};
use teos_common::cryptography;
use teos_common::receipts::RegistrationReceipt;
use teos_common::UserId;

use crate::dbm::DBM;
use crate::extended_appointment::{compute_appointment_slots, ExtendedAppointment, UUID};

/// Data regarding a user subscription with the tower.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    /// Number of appointment slots available for a given user.
    pub(crate) available_slots: u32,
    /// Block height where the user subscription will expire.
    pub(crate) subscription_expiry: u32,
    /// Map of appointment ids and the how many slots they take from the subscription.
    pub(crate) appointments: HashMap<UUID, u32>,
}

impl UserInfo {
    /// Creates a new [UserInfo] instance.
    pub fn new(available_slots: u32, subscription_expiry: u32) -> Self {
        UserInfo {
            available_slots,
            subscription_expiry,
            appointments: HashMap::new(),
        }
    }
}

/// Error raised if the user cannot be authenticated.
#[derive(Debug, PartialEq)]
pub struct AuthenticationFailure<'a>(&'a str);

/// Error raised if the user subscription has not enough slots to fit a new appointment.
#[derive(Debug, PartialEq)]
pub struct NotEnoughSlots;

/// Error raised if the user subscription slots limit has been reached.
///
/// This is currently set to [u32::MAX].
#[derive(Debug, PartialEq)]
pub struct MaxSlotsReached;

/// Component in charge of managing access to the tower resources.
///
/// The [Gatekeeper] keeps track of user subscriptions and allow users to interact with the tower based on it.
/// A user is only allowed to send/request data to/from the tower given they have an ongoing subscription with
/// available slots.
/// This is the only component in the system that has some knowledge regarding users, all other components do query the
/// [Gatekeeper] for such information.
//TODO: Check if calls to the Gatekeeper need explicit Mutex of if Rust already prevents race conditions in this case.
pub struct Gatekeeper {
    /// last known block header by the [Gatekeeper].
    last_known_block_header: ValidatedBlockHeader,
    /// Number of slots new subscriptions get by default.
    subscription_slots: u32,
    /// Expiry time new subscription get by default, in blocks (starting from the block the subscription is requested).
    subscription_duration: u32,
    /// Grace period given to renew subscriptions, in blocks.
    expiry_delta: u32,
    /// Map of users registered within the tower.
    pub(crate) registered_users: RefCell<HashMap<UserId, UserInfo>>,
    /// Map of users whose subscription has been outdated. Kept around so other components can perform the necessary
    /// cleanups when deleting data.
    pub(crate) outdated_users_cache: RefCell<HashMap<u32, HashMap<UserId, Vec<UUID>>>>,
    /// A [DBM] (database manager) instance. Used to persist appointment data into disk.
    dbm: Arc<Mutex<DBM>>,
}

impl Gatekeeper {
    /// Creates a new [Gatekeeper] instance.
    pub fn new(
        last_known_block_header: ValidatedBlockHeader,
        subscription_slots: u32,
        subscription_duration: u32,
        expiry_delta: u32,
        dbm: Arc<Mutex<DBM>>,
    ) -> Self {
        Gatekeeper {
            last_known_block_header,

            subscription_slots,
            subscription_duration,
            expiry_delta,
            registered_users: RefCell::new(HashMap::new()),
            outdated_users_cache: RefCell::new(HashMap::new()),
            dbm,
        }
    }

    /// Authenticates a user.
    ///
    /// User authentication is performed using ECRecover against fixed messages (one for each command).
    /// Notice all interaction with the tower should be guarded by this.
    pub fn authenticate_user(
        &self,
        message: &[u8],
        signature: &str,
    ) -> Result<UserId, AuthenticationFailure> {
        let user_id = UserId(
            cryptography::recover_pk(message, signature)
                .map_err(|_| AuthenticationFailure("Wrong message or signature."))?,
        );

        if self.registered_users.borrow().contains_key(&user_id) {
            Ok(user_id)
        } else {
            Err(AuthenticationFailure("User not found."))
        }
    }

    /// Adds a new user to the tower (or updates its subscription if already registered).
    pub fn add_update_user(&self, user_id: UserId) -> Result<RegistrationReceipt, MaxSlotsReached> {
        let block_count = self.last_known_block_header.height;

        // TODO: For now, new calls to `add_update_user` add subscription_slots to the current count and reset the expiry time
        let mut borrowed = self.registered_users.borrow_mut();
        let user_info = match borrowed.get_mut(&user_id) {
            // User already exists, updating the info
            Some(user_info) => {
                user_info.available_slots = user_info
                    .available_slots
                    .checked_add(self.subscription_slots)
                    .ok_or(MaxSlotsReached)?;
                user_info.subscription_expiry = block_count + self.subscription_duration;
                self.dbm.lock().unwrap().update_user(user_id, &user_info);

                user_info
            }
            // New user
            None => {
                let user_info = UserInfo::new(
                    self.subscription_slots,
                    block_count + self.subscription_duration,
                );
                self.dbm
                    .lock()
                    .unwrap()
                    .store_user(user_id, &user_info)
                    .unwrap();

                borrowed.insert(user_id, user_info);
                borrowed.get_mut(&user_id).unwrap()
            }
        };

        Ok(RegistrationReceipt::new(
            user_id,
            user_info.available_slots,
            user_info.subscription_expiry,
        ))
    }

    /// Adds an appointment to a given user, or updates it if already present in the system (and belonging to the requester).
    pub fn add_update_appointment(
        &self,
        user_id: UserId,
        uuid: UUID,
        appointment: &ExtendedAppointment,
    ) -> Result<u32, NotEnoughSlots> {
        // For updates, the difference between the existing appointment size and the update is computed.
        let mut borrowed = self.registered_users.borrow_mut();
        let user_info = borrowed.get_mut(&user_id).unwrap();
        let used_slots = user_info.appointments.get(&uuid).map_or(0, |x| *x);

        let required_slots =
            compute_appointment_slots(appointment.encrypted_blob().len(), ENCRYPTED_BLOB_MAX_SIZE);

        let diff = required_slots as i64 - used_slots as i64;
        if diff <= user_info.available_slots as i64 {
            // Filling / freeing slots depending on whether this is an update or not, and if it is bigger or smaller
            // than the old appointment
            user_info.appointments.insert(uuid, required_slots);
            user_info.available_slots = (user_info.available_slots as i64 - diff) as u32;

            self.dbm.lock().unwrap().update_user(user_id, &user_info);

            Ok(user_info.available_slots)
        } else {
            Err(NotEnoughSlots)
        }
    }

    /// Checks whether a subscription has expired.
    pub fn has_subscription_expired(
        &self,
        user_id: UserId,
    ) -> Result<(bool, u32), AuthenticationFailure<'_>> {
        self.registered_users.borrow().get(&user_id).map_or(
            Err(AuthenticationFailure("User not found.")),
            |user_info| {
                Ok((
                    self.last_known_block_header.height >= user_info.subscription_expiry,
                    user_info.subscription_expiry,
                ))
            },
        )
    }

    /// Gets a map of outdated users. Outdated users are those whose subscription has expired and the renewal grace period
    /// has already passed ([expiry_delta](Self::expiry_delta)).
    ///
    /// The data is pulled from the cache if present, otherwise it is computed on the fly.
    pub fn get_outdated_users(&self, block_height: u32) -> HashMap<UserId, Vec<UUID>> {
        let borrowed = self.outdated_users_cache.borrow();
        match borrowed.get(&block_height) {
            Some(users) => users.clone(),
            None => {
                let mut users = HashMap::new();
                for (user_id, user_info) in self.registered_users.borrow().iter() {
                    if block_height == user_info.subscription_expiry + self.expiry_delta {
                        users.insert(*user_id, user_info.appointments.keys().cloned().collect());
                    }
                }

                users
            }
        }
    }

    /// Gets a list of outdated user ids.
    pub fn get_outdated_user_ids(&self, block_height: u32) -> Vec<UserId> {
        self.get_outdated_users(block_height)
            .keys()
            .cloned()
            .collect()
    }

    /// Get a map of outdated appointments (from any user).
    pub fn get_outdated_appointments(&self, block_height: u32) -> HashSet<UUID> {
        HashSet::from_iter(
            self.get_outdated_users(block_height)
                .into_values()
                .flatten(),
        )
    }

    /// Updates the outdated users cache by adding new outdated users (for a given height) and deleting old ones if the cache
    /// grows beyond it's maximum size.
    pub fn update_outdated_users_cache(&self, block_height: u32) -> HashMap<UserId, Vec<UUID>> {
        let mut outdated_users = HashMap::new();

        if !self
            .outdated_users_cache
            .borrow()
            .contains_key(&block_height)
        {
            outdated_users = self.get_outdated_users(block_height);
            let mut borrowed = self.outdated_users_cache.borrow_mut();
            borrowed.insert(block_height.clone(), outdated_users.clone());

            // Remove the first entry from the cache if it grows beyond the limit size
            if borrowed.len() > OUTDATED_USERS_CACHE_SIZE_BLOCKS {
                // TODO: This may be simpler using BTreeMaps once first_entry is not nightly anymore
                let mut keys = borrowed.keys().to_owned().collect::<Vec<&u32>>();
                keys.sort();
                let first = keys[0].clone();

                // Remove data from the cache and from the database
                // TODO: This can be implemented as a batch delete
                borrowed.remove(&first).map(|users| {
                    for user_id in users.keys() {
                        self.dbm.lock().unwrap().remove_user(*user_id);
                    }
                });
            }
        }

        outdated_users
    }

    /// Deletes a collection of appointments from the users' subscriptions (both from memory and from the database).
    ///
    /// Notice appointments are only de-linked from users, but not actually removed. This is because the [Gatekeeper]
    /// does not actually hold any [ExtendedAppointment](crate::extended_appointment::ExtendedAppointment) data,
    /// just references to them (the same applies to the database).
    pub fn delete_appointments(&self, appointments: &HashMap<UUID, UserId>) {
        let mut updated_users = HashSet::new();

        for (uuid, user_id) in appointments {
            // Remove the appointment from the appointment list and update the available slots
            self.registered_users
                .borrow_mut()
                .get_mut(&user_id)
                .map(|user_info| {
                    user_info
                        .appointments
                        .remove(uuid)
                        .map(|x| user_info.available_slots += x);
                    updated_users.insert(user_id);
                });
        }

        // Update data in the database
        for user_id in updated_users {
            self.dbm.lock().unwrap().update_user(
                *user_id,
                self.registered_users.borrow().get(user_id).unwrap(),
            );
        }
    }
}

impl chain::Listen for Gatekeeper {
    /// Handles the monitoring process by the [Gatekeeper].
    ///
    /// This is mainly used to keep track of time and expire / outdate subscriptions when needed.
    fn block_connected(&self, block: &bitcoin::Block, height: u32) {
        // Expired user deletion is delayed. Users are deleted when their subscription is outdated, not expired.
        log::info!("New block received: {}", block.block_hash());
        let outdated_users = self.update_outdated_users_cache(height);

        for user_id in outdated_users.keys() {
            self.registered_users.borrow_mut().remove(user_id);
        }
    }

    /// FIXME: To be implemented.
    /// This will handle reorgs on the [Gatekeeper].
    #[allow(unused_variables)]
    fn block_disconnected(&self, header: &bitcoin::BlockHeader, height: u32) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dbm::Error as DBError;
    use crate::test_utils::{
        generate_dummy_appointment, generate_dummy_appointment_with_user, generate_uuid,
        get_random_user_id, Blockchain,
    };
    use lightning::chain::Listen;
    use teos_common::cryptography::{get_random_bytes, get_random_keypair};

    const SLOTS: u32 = 21;
    const DURATION: u32 = 500;
    const EXPIRY_DELTA: u32 = 42;
    const START_HEIGHT: usize = 100;

    #[test]
    fn test_authenticate_user() {
        let chain = Blockchain::default().with_height(START_HEIGHT);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm);

        // Authenticate user returns the UserId if the user is found in the system, or an AuthenticationError otherwise.

        // Let's first check with an unknown user
        let message = "message".as_bytes();
        let wrong_signature = "signature";
        assert_eq!(
            gatekeeper.authenticate_user(message, wrong_signature),
            Err(AuthenticationFailure("Wrong message or signature."))
        );

        // Let's now provide data generated by an actual user, still the user is unknown
        let (user_sk, user_pk) = get_random_keypair();
        let signature = cryptography::sign(message, &user_sk).unwrap();
        assert_eq!(
            gatekeeper.authenticate_user(message, &signature),
            Err(AuthenticationFailure("User not found."))
        );

        // Last, let's add the user to the Gatekeeper and try again.
        let user_id = UserId(user_pk);
        gatekeeper.add_update_user(user_id).unwrap();
        assert_eq!(
            gatekeeper.authenticate_user(message, &signature),
            Ok(user_id)
        );
    }

    #[test]
    fn test_add_update_user() {
        let mut chain = Blockchain::default().with_height(START_HEIGHT);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let mut gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm.clone());

        // add_update_user adds a user to the system if it is not still registered, otherwise it add slots to the user subscription
        // and refreshes the subscription expiry. Slots are added up to u32:MAX, further call will return an MaxSlotsReached error.

        // Let's start by adding new user
        let user_id = get_random_user_id();
        let receipt = gatekeeper.add_update_user(user_id).unwrap();
        // The data should have been also added to the database
        assert_eq!(
            dbm.lock().unwrap().load_user(user_id).unwrap(),
            UserInfo::new(receipt.available_slots(), receipt.subscription_expiry())
        );

        // Let generate a new block and add the user again to check that both the slots and expiry are updated.
        chain.generate_with_txs(Vec::new());
        gatekeeper.last_known_block_header = chain.tip();
        let updated_receipt = gatekeeper.add_update_user(user_id).unwrap();

        assert_eq!(
            updated_receipt.available_slots(),
            receipt.available_slots() * 2
        );
        assert_eq!(
            updated_receipt.subscription_expiry(),
            receipt.subscription_expiry() + 1
        );

        // Data in the database should have been updated too
        assert_eq!(
            dbm.lock().unwrap().load_user(user_id).unwrap(),
            UserInfo::new(
                updated_receipt.available_slots(),
                updated_receipt.subscription_expiry()
            )
        );

        // If the slot count reaches u32::MAX we should receive an error
        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user_id)
            .unwrap()
            .available_slots = u32::MAX;

        assert!(matches!(
            gatekeeper.add_update_user(user_id),
            Err(MaxSlotsReached)
        ));

        // Data in the database remains untouched
        assert_eq!(
            dbm.lock().unwrap().load_user(user_id).unwrap(),
            UserInfo::new(
                updated_receipt.available_slots(),
                updated_receipt.subscription_expiry()
            )
        );
    }

    #[test]
    fn test_add_update_appointment() {
        let chain = Blockchain::default().with_height(START_HEIGHT);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm.clone());

        // if a given appointment is not associated with a given user, add_update_appointment adds the appointment user appointments alongside the number os slots it consumes. If the appointment
        // is already associated with the user, it will update it (both data and slot count).

        // Let's first add the a user to the Gatekeeper (inputs are always sanitized here, so we don't need tests for non-registered users)
        let user_id = get_random_user_id();
        gatekeeper.add_update_user(user_id).unwrap();

        // Now let's add a new appointment
        let slots_before = gatekeeper
            .registered_users
            .borrow()
            .get(&user_id)
            .unwrap()
            .available_slots;
        let (uuid, appointment) = generate_dummy_appointment_with_user(user_id, None);
        let available_slots = gatekeeper
            .add_update_appointment(user_id, uuid, &appointment)
            .unwrap();

        assert!(gatekeeper.registered_users.borrow()[&user_id]
            .appointments
            .contains_key(&uuid));
        assert_eq!(slots_before, available_slots + 1);

        // Slots should have been updated in the database too. Notice the appointment won't be there yet
        // given the Watcher is responsible for adding it, and it will do so after calling this method
        let mut loaded_user = dbm.lock().unwrap().load_user(user_id).unwrap();
        assert_eq!(loaded_user.available_slots, available_slots);

        // Adding the exact same appointment should leave the slots count unchanged
        let mut updated_slot_count = gatekeeper
            .add_update_appointment(user_id, uuid, &appointment)
            .unwrap();
        assert!(gatekeeper.registered_users.borrow()[&user_id]
            .appointments
            .contains_key(&uuid));
        assert_eq!(updated_slot_count, available_slots);
        loaded_user = dbm.lock().unwrap().load_user(user_id).unwrap();
        assert_eq!(loaded_user.available_slots, updated_slot_count);

        // If we add an update to an existing appointment with a bigger data blob (modulo ENCRYPTED_BLOB_MAX_SIZE), additional slots should be taken
        let mut bigger_appointment = appointment.clone();
        bigger_appointment.inner.encrypted_blob = get_random_bytes(ENCRYPTED_BLOB_MAX_SIZE + 1);
        updated_slot_count = gatekeeper
            .add_update_appointment(user_id, uuid, &bigger_appointment)
            .unwrap();
        assert!(gatekeeper.registered_users.borrow()[&user_id]
            .appointments
            .contains_key(&uuid));
        assert_eq!(updated_slot_count, available_slots - 1);
        loaded_user = dbm.lock().unwrap().load_user(user_id).unwrap();
        assert_eq!(loaded_user.available_slots, updated_slot_count);

        // Adding back a smaller update (modulo ENCRYPTED_BLOB_MAX_SIZE) should reduce the count
        updated_slot_count = gatekeeper
            .add_update_appointment(user_id, uuid, &appointment)
            .unwrap();
        assert!(gatekeeper.registered_users.borrow()[&user_id]
            .appointments
            .contains_key(&uuid));
        assert_eq!(updated_slot_count, available_slots);
        loaded_user = dbm.lock().unwrap().load_user(user_id).unwrap();
        assert_eq!(loaded_user.available_slots, updated_slot_count);

        // Adding an appointment with a different uuid should not count as an update
        let new_uuid = generate_uuid();
        updated_slot_count = gatekeeper
            .add_update_appointment(user_id, new_uuid, &appointment)
            .unwrap();
        assert!(gatekeeper.registered_users.borrow()[&user_id]
            .appointments
            .contains_key(&new_uuid));
        assert_eq!(updated_slot_count, available_slots - 1);
        loaded_user = dbm.lock().unwrap().load_user(user_id).unwrap();
        assert_eq!(loaded_user.available_slots, updated_slot_count);

        // Finally, trying to add an appointment when the user has no enough slots should fail
        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user_id)
            .unwrap()
            .available_slots = 0;
        assert!(matches!(
            gatekeeper.add_update_appointment(user_id, generate_uuid(), &appointment),
            Err(NotEnoughSlots)
        ));
        // The entry in the database should remain unchanged in this case
        loaded_user = dbm.lock().unwrap().load_user(user_id).unwrap();
        assert_eq!(loaded_user.available_slots, updated_slot_count);
    }

    #[test]
    fn test_has_subscription_expired() {
        let chain = Blockchain::default().with_height(START_HEIGHT);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm);

        let user_id = get_random_user_id();

        // If the user is not registered, querying for a subscription expiry check should return an error
        assert!(matches!(
            gatekeeper.has_subscription_expired(user_id),
            Err(AuthenticationFailure { .. })
        ));

        // If the user is registered and the subscription is active we should get (false, expiry)
        gatekeeper.add_update_user(user_id).unwrap();
        assert_eq!(
            gatekeeper.has_subscription_expired(user_id),
            Ok((false, DURATION + START_HEIGHT as u32))
        );

        // If the subscription has expired, we should get (true, expiry). Let's modify the user entry
        let expiry = START_HEIGHT as u32;
        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user_id)
            .unwrap()
            .subscription_expiry = expiry;
        assert_eq!(
            gatekeeper.has_subscription_expired(user_id),
            Ok((true, expiry))
        );
    }

    #[test]
    fn test_get_outdated_users() {
        let start_height = START_HEIGHT as u32 + EXPIRY_DELTA;
        let chain = Blockchain::default().with_height(start_height as usize);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm);

        // Initially, the outdated_users_cache is empty, so querying any block height should return an empty map
        for i in 0..start_height {
            assert_eq!(gatekeeper.get_outdated_users(i).len(), 0);
        }

        // Adding a user whose subscription is outdated should return an entry
        let user_id = get_random_user_id();
        gatekeeper.add_update_user(user_id).unwrap();

        // Add also an appointment so we can check the returned data
        let appointment = generate_dummy_appointment(None);
        let uuid = generate_uuid();
        gatekeeper
            .add_update_appointment(user_id, uuid, &appointment)
            .unwrap();

        // Check that data is not in the cache before querying
        assert_eq!(gatekeeper.outdated_users_cache.borrow().len(), 0);

        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user_id)
            .unwrap()
            .subscription_expiry = START_HEIGHT as u32;

        let outdated_users = gatekeeper.get_outdated_users(start_height);
        assert_eq!(outdated_users.len(), 1);
        assert_eq!(outdated_users[&user_id], Vec::from([uuid]));

        // If the outdated_users_cache has an entry, the data will be returned straightaway instead of computed
        // on the fly
        let target_height = 2;
        assert_eq!(
            gatekeeper.outdated_users_cache.borrow().get(&target_height),
            None
        );
        assert_eq!(gatekeeper.get_outdated_users(target_height), HashMap::new());

        let mut hm = HashMap::new();
        hm.insert(user_id, Vec::from([uuid]));
        gatekeeper
            .outdated_users_cache
            .borrow_mut()
            .insert(target_height, hm.clone());
        assert_eq!(gatekeeper.get_outdated_users(start_height), hm);
    }

    #[test]
    fn test_get_outdated_appointments() {
        let start_height = START_HEIGHT as u32 + EXPIRY_DELTA;
        let chain = Blockchain::default().with_height(start_height as usize);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm);

        // get_outdated_appointments returns a list of appointments that were outdated at a given block height, indistinguishably of their user.

        // If there are no outdated users, there cannot be outdated appointments
        for i in 0..start_height {
            assert_eq!(gatekeeper.get_outdated_appointments(i).len(), 0);
        }

        // Adding data about different users and appointments should return a flattened list of appointments
        let user1_id = get_random_user_id();
        let user2_id = get_random_user_id();
        gatekeeper.add_update_user(user1_id).unwrap();
        gatekeeper.add_update_user(user2_id).unwrap();

        // Manually set the user expiry for the test
        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user1_id)
            .unwrap()
            .subscription_expiry = START_HEIGHT as u32;

        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user2_id)
            .unwrap()
            .subscription_expiry = START_HEIGHT as u32;

        let uuid1 = generate_uuid();
        let uuid2 = generate_uuid();
        let appointment = generate_dummy_appointment(None);

        gatekeeper
            .add_update_appointment(user1_id, uuid1, &appointment)
            .unwrap();
        gatekeeper
            .add_update_appointment(user2_id, uuid2, &appointment)
            .unwrap();

        let outdated_appointments = gatekeeper.get_outdated_appointments(start_height);
        assert_eq!(outdated_appointments.len(), 2);
        assert!(outdated_appointments.contains(&uuid1));
        assert!(outdated_appointments.contains(&uuid2));
    }

    #[test]
    fn test_update_outdated_users_cache() {
        let start_height = START_HEIGHT as u32 + EXPIRY_DELTA;
        let chain = Blockchain::default().with_height(start_height as usize);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm.clone());

        // update_outdated_users_cache adds the users that get outdated at a given block height to the cache and removes the oldest
        // entry once the cache has reached it's maximum size.

        // If there's outdated data to be added and there's room in the cache, the data will be added
        let user_id = get_random_user_id();
        gatekeeper.add_update_user(user_id).unwrap();
        gatekeeper
            .registered_users
            .borrow_mut()
            .get_mut(&user_id)
            .unwrap()
            .subscription_expiry = start_height - EXPIRY_DELTA - 1;

        assert_eq!(gatekeeper.outdated_users_cache.borrow().len(), 0);
        gatekeeper.update_outdated_users_cache(start_height - 1);
        assert_eq!(gatekeeper.outdated_users_cache.borrow().len(), 1);

        // If the cache has room and there's no data to add, an empty entry will be added
        gatekeeper.update_outdated_users_cache(start_height);
        assert_eq!(gatekeeper.outdated_users_cache.borrow().len(), 2);
        assert_eq!(
            gatekeeper.outdated_users_cache.borrow()[&(start_height)],
            HashMap::new()
        );

        // Adding data (even empty) to the cache up to it's limit should remove the first element
        for i in start_height + 1..start_height + OUTDATED_USERS_CACHE_SIZE_BLOCKS as u32 - 1 {
            gatekeeper.update_outdated_users_cache(i);
        }

        // Check the first key is still there and that the user can still be found in the database
        assert_eq!(
            gatekeeper.outdated_users_cache.borrow().len(),
            OUTDATED_USERS_CACHE_SIZE_BLOCKS
        );
        assert!(gatekeeper
            .outdated_users_cache
            .borrow()
            .contains_key(&(start_height - 1)));
        assert!(matches!(
            dbm.lock().unwrap().load_user(user_id),
            Ok(UserInfo { .. })
        ));

        // Add one more block and check again. Data should have been removed from the cache and the database
        gatekeeper.update_outdated_users_cache(
            start_height + OUTDATED_USERS_CACHE_SIZE_BLOCKS as u32 - 1,
        );
        assert_eq!(
            gatekeeper.outdated_users_cache.borrow().len(),
            OUTDATED_USERS_CACHE_SIZE_BLOCKS
        );
        assert!(!gatekeeper
            .outdated_users_cache
            .borrow()
            .contains_key(&(start_height - 1)));
        assert!(matches!(dbm.lock().unwrap().load_user(user_id), Err(..)));
    }

    #[test]
    fn test_delete_appointments() {
        let chain = Blockchain::default().with_height(START_HEIGHT);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm.clone());

        // delete_appointments will remove a list of appointments from the Gatekeeper (as long as they exist)
        let mut all_appointments = HashMap::new();
        let mut to_be_deleted = HashMap::new();
        let mut rest = HashMap::new();
        for i in 1..11 {
            let user_id = get_random_user_id();
            let uuid = generate_uuid();
            all_appointments.insert(uuid, user_id);

            if i % 2 == 0 {
                to_be_deleted.insert(uuid, user_id);
            } else {
                rest.insert(uuid, user_id);
            }
        }

        // Calling the method with unknown data should work but do nothing
        assert!(gatekeeper.registered_users.borrow().is_empty());
        gatekeeper.delete_appointments(&all_appointments);
        assert!(gatekeeper.registered_users.borrow().is_empty());

        // If there's matching data in the gatekeeper it should be deleted
        for (uuid, user_id) in to_be_deleted.iter() {
            gatekeeper.add_update_user(*user_id).unwrap();
            gatekeeper
                .add_update_appointment(*user_id, *uuid, &generate_dummy_appointment(None))
                .unwrap();
        }

        // Check before deleting
        assert_eq!(gatekeeper.registered_users.borrow().len(), 5);
        for (uuid, user_id) in to_be_deleted.iter() {
            assert!(gatekeeper.registered_users.borrow()[user_id]
                .appointments
                .contains_key(uuid));

            // The slot count should be decreased now too (both in memory and in the database)
            assert_ne!(
                gatekeeper.registered_users.borrow()[user_id].available_slots,
                gatekeeper.subscription_slots
            );
            assert_ne!(
                gatekeeper
                    .dbm
                    .lock()
                    .unwrap()
                    .load_user(*user_id)
                    .unwrap()
                    .available_slots,
                gatekeeper.subscription_slots
            );
        }
        for (_, user_id) in rest.iter() {
            assert!(!gatekeeper.registered_users.borrow().contains_key(user_id));
        }

        // And after
        gatekeeper.delete_appointments(&all_appointments);
        for (uuid, user_id) in to_be_deleted.iter() {
            assert!(!gatekeeper.registered_users.borrow()[&user_id]
                .appointments
                .contains_key(uuid));

            // The slot count is back to default
            assert_eq!(
                gatekeeper.registered_users.borrow()[&user_id].available_slots,
                gatekeeper.subscription_slots
            );
            assert_eq!(
                gatekeeper
                    .dbm
                    .lock()
                    .unwrap()
                    .load_user(*user_id)
                    .unwrap()
                    .available_slots,
                gatekeeper.subscription_slots
            );
        }
        for (_, user_id) in rest.iter() {
            assert!(!gatekeeper.registered_users.borrow().contains_key(user_id));
        }
    }

    #[test]
    fn test_block_connected() {
        // block_connected in the Gatekeeper is used to keep track of time in order to manage the users' subscription expiry.
        // When a new block is received, the outdated_users_cache is updated and the users outdated in the given height are
        // deleted from registered_users.

        let chain = Blockchain::default().with_height(START_HEIGHT);
        let tip = chain.tip();
        let dbm = Arc::new(Mutex::new(DBM::in_memory().unwrap()));
        let gatekeeper = Gatekeeper::new(tip, SLOTS, DURATION, EXPIRY_DELTA, dbm.clone());
        let mut last_height = tip.height;

        // Check that the cache is being updated when blocks are being received (even with empty data) and it's max size is not exceeded
        for i in 0..OUTDATED_USERS_CACHE_SIZE_BLOCKS * 2 {
            last_height += 1;
            gatekeeper.block_connected(chain.blocks.last().unwrap(), last_height);
            if i < OUTDATED_USERS_CACHE_SIZE_BLOCKS {
                assert_eq!(gatekeeper.outdated_users_cache.borrow().len(), i + 1)
            } else {
                assert_eq!(
                    gatekeeper.outdated_users_cache.borrow().len(),
                    OUTDATED_USERS_CACHE_SIZE_BLOCKS
                )
            }
        }

        // Check that users are outdated when the expected height if hit
        let user1_id = get_random_user_id();
        let user2_id = get_random_user_id();
        let user3_id = get_random_user_id();

        last_height += 1;
        for user in vec![user1_id, user2_id, user3_id] {
            gatekeeper.add_update_user(user).unwrap();
            gatekeeper
                .registered_users
                .borrow_mut()
                .get_mut(&user)
                .unwrap()
                .subscription_expiry = last_height as u32 - EXPIRY_DELTA;
        }

        // Connect a new block so users are included in the cache
        gatekeeper.block_connected(chain.blocks.last().unwrap(), last_height as u32);

        // Check that users have been added to the cache and removed from registered_users
        for user in vec![user1_id, user2_id, user3_id] {
            assert!(gatekeeper.outdated_users_cache.borrow()[&last_height].contains_key(&user));
            assert!(!gatekeeper.registered_users.borrow().contains_key(&user));

            // Data is still in the database since the user is in the cache
            assert!(matches!(
                dbm.lock().unwrap().load_user(user),
                Ok(UserInfo { .. })
            ));
        }

        // Perform a full cache rotation and check that the data is not there anymore
        for _ in 0..OUTDATED_USERS_CACHE_SIZE_BLOCKS {
            last_height += 1;
            gatekeeper.block_connected(chain.blocks.last().unwrap(), last_height);
        }

        for user in vec![user1_id, user2_id, user3_id] {
            assert!(matches!(
                dbm.lock().unwrap().load_user(user),
                Err(DBError::NotFound)
            ));
        }
    }
}
